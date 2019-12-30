package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"sync"
	"time"

	"github.com/Noah-Huppert/goconf"
	"github.com/Noah-Huppert/golog"
	redislib "github.com/go-redis/redis/v7"
	"github.com/gorilla/mux"
	"github.com/gorilla/websocket"
	"gopkg.in/go-playground/validator.v9"
)

// config for app
type config struct {
	// APIAddr addres to run HTTP API
	APIAddr string `default:":5000" validate:"required"`

	// Redis address
	RedisAddr string `default:"localhost:6379" validate:"required"`
}

// datastore defines methods used to store data in a database
type datastore interface {
	// Write data of a type under a name to the database
	Write(dataType, name string, data interface{}) error

	// Read data of a type by name from a database. Returns DataNotFoundErr if
	// data of the specified type and key does not exist.
	Read(dataType, name string, data interface{}) error

	// Exists determines if data of a type is stored under the provided name
	Exists(dataType, name string) (bool, error)
}

// DataNotFoundErr indicates no data was found by the datastore interface
var DataNotFoundErr error = fmt.Errorf("data not found")

// redisDatastore implements datastore with redis. Data is seralized using JSON.
// Redis keys are in the format: dataType + ":" + name
type redisDatastore struct {
	redis *redislib.Client
}

// Write stores data under a key
func (rd redisDatastore) Write(dataType, name string, data interface{}) error {
	marshalled, err := json.Marshal(data)
	if err != nil {
		return fmt.Errorf("failed to encode data as JSON: %s", err)
	}

	status := rd.redis.Set(fmt.Sprintf("%s:%s", dataType, name), marshalled, 0)
	if status.Err() != nil {
		return fmt.Errorf("failed to store in redis: %s", err)
	}

	return nil
}

// Read retrieves data by a key
func (rd redisDatastore) Read(dataType, name string, data interface{}) error {
	result := rd.redis.Get(fmt.Sprintf("%s:%s", dataType, name))
	if result == nil {
		return DataNotFoundErr
	}

	if result.Err() != nil {
		return fmt.Errorf("failed to get data from redis: %s", result.Err())
	}

	if err := json.Unmarshal([]byte(result.Val()), data); err != nil {
		return fmt.Errorf("failed to decode data as JSON: %s", err)
	}

	return nil
}

// Exists determines if a key exists
func (rd redisDatastore) Exists(dataType, name string) (bool, error) {
	result := rd.redis.Exists(fmt.Sprintf("%s:%s", dataType, name))
	if result.Err() != nil {
		return false, fmt.Errorf("failed to query redis for key: %s",
			result.Err())
	}

	return result.Val() == 1, nil
}

// syncSess is a session which clients can join to synchronize their watching of
// a common video.
type syncSess struct {
	// ID of session
	ID string `json:"id"`

	// Name of session
	Name string `json:"name"`

	// State of common video being synchronized among clients
	State syncState
}

// syncState stores the state of a common video which multiple clients
// are watching.
//
// Field "validate" tags are used to validate messages sent to the server.
type syncState struct {
	// Playing indicates if the video is currently playing, false means the
	// video is paused.
	Playing bool `json:"playing" validate:"required"`

	// SecondsProgressed indicates the current position of the video via the
	// number of seconds which have progressed
	SecondsProgressed uint `json:"seconds_progressed" validate:"required"`
}

// These constants are the names of data types which can be stored in
// the database
const (
	syncSessT string = "session"
)

// baseHdlr has useful helpers
type baseHdlr struct {
	ctx context.Context
	log golog.Logger
	db  datastore
}

// defHttpErr
var defHttpErr []byte = []byte("{\"error\": \"internal server error\"}")

// httpErr returns a JSON error response and logs.
// Set status to 0 for default response HTTP status
// of http.StatusInternalServerError.
func (h baseHdlr) httpErr(w http.ResponseWriter, status int, err,
	inErr error) {
	if status == 0 {
		status = http.StatusInternalServerError
	}
	var resp []byte = defHttpErr
	if err != nil {
		resp = []byte(fmt.Sprintf("{\"error\": \"%s\"}", err))
	}

	if _, wErr := w.Write(resp); wErr != nil {
		h.log.Errorf("failed to write error http response: %s", wErr)
	}
	w.WriteHeader(status)

	h.log.Errorf("http endpoint error: %s:%s", err, inErr)
}

// httpJSON returns a JSON response
func (h baseHdlr) httpJSON(w http.ResponseWriter, d interface{}) {
	w.Header().Set("Content-Type", "application/json")
	e := json.NewEncoder(w)
	if err := e.Encode(d); err != nil {
		h.httpErr(w, 0, nil, fmt.Errorf("failed to write JSON http "+
			"response, data=%#v: %s", d, err))
		return
	}
}

// logHdlr logs every request then passes the request to a child handler
type logHdlr struct {
	log golog.Logger

	// childHdlr is the handler which will actually handle the request
	childHdlr http.Handler
}

// ServeHTTP logs and forwards the request
func (h logHdlr) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.log.Debugf("method=%s path=%s time=%s", r.Method, r.URL, time.Now())
	h.childHdlr.ServeHTTP(w, r)
}

// healthHdlr responds so clients known the server is operational
type healthHdlr struct {
	baseHdlr
}

// ServeHTTP responds with the "ok" key set to "true"
func (h healthHdlr) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.httpJSON(w, map[string]bool{
		"ok": true,
	})
}

// syncWS upgrades connections to web sockets and uses a custom protocol to
// keep client's videos in sync.
///
// Data is encoded in JSON on the web socket.
// All messages must have a "type" field. Depending on the type additional
// fields may be required. Different message types can only be sent by the
// server or the client.
//
// Client->server message types:
//
//    - create-session: Request to create a sync session, see wsCreateSessMsg
//                      for required message fields, the server will respond
//                      with a hereis-session message.
//    - report-state: Provides server with information about the client's
//                    current video player state, see wsReportStateMsg for
//                    required message fields
//    - change-state: Modifies a sync session's state, causing all clients in
//                    the session to update to this state, see wsChangeStateMsg
//                    for required fields.
//
// Server->client message types:
//
//    - hereis-session: Provides a client with their current sync session, see
//                      wsHereisSessMsg for message fields.
//    - command-state: Tells a client what its state should be to be in sync
//                     with others in the session, see wsCmdStateMsg for
//                     required fields.
//    - error: Indicates an error occured, see wsErrMsg for message fields.
type syncWS struct {
	baseHdlr

	// upgrader is used to upgrade HTTP connections to to web
	// socket connections
	upgrader websocket.Upgrader

	// validate is a validator used to ensure messages sent by over web socket
	// clients are in the correct format. Lazy initialized by .wsParseJSON().
	validate *validator.Validate
}

// wsMsg identifies messages so they can be handled
type wsMsg struct {
	Type string `json:"type"`
}

// wsMsg.Type constants, see syncWs docs
const (
	wsErrMsgT         string = "error"
	wsHereisSessMsgT         = "hereis-session"
	wsCreateSessMsgT         = "create-session"
	wsReportStateMsgT        = "report-state"
	wsCmdStateMsgT           = "command-state"
	wsChangeStateMsgT        = "change-state"
)

// wsErrMsg is an error message which could be sent via a web socket
type wsErrMsg struct {
	wsMsg

	// Err is the error which occurred
	Err string `json:"error"`
}

// newWsErrMsg initializes a wsErrMsg
func newWsErrMsg() wsErrMsg {
	return wsErrMsg{
		wsMsg: wsMsg{
			Type: wsErrMsgT,
		},
	}
}

// wsCreateSessMsg contains details about a new sync session
type wsCreateSessMsg struct {
	wsMsg

	// Name of new sync session
	Name string `json:"name" validate:"required"`
}

// newWsCreateSessMsg initializes a new wsCreateSessMsg
func newWsCreateSessMsg() wsCreateSessMsg {
	return wsCreateSessMsg{
		wsMsg: wsMsg{
			Type: wsCreateSessMsgT,
		},
	}
}

// wsReportStateMsg contains details about a client's current video state
type wsReportStateMsg struct {
	wsMsg

	// State is the current state of a client's video
	State syncState `json:"state" validate:"required"`
}

// newWsReportStateMsg initializes a wsReportStateMsg
func newWsReportStateMsg() wsReportStateMsg {
	return wsReportStateMsg{
		wsMsg: wsMsg{
			Type: wsReportStateMsgT,
		},
	}
}

// wsChangeStateMsg contains details about a client's new desired state for
// a video
type wsChangeStateMsg struct {
	wsMsg

	// State is the client's new desired video state
	State syncState `json:"state" validate:"required"`
}

// newWsChangeStateMsg initializes a new wsChangeStateMsg
func newWsChangeStateMsg() wsChangeStateMsg {
	return wsChangeStateMsg{
		wsMsg: wsMsg{
			Type: wsChangeStateMsgT,
		},
	}
}

// wsHereisSessMsg contains a client's sync session
type wsHereisSessMsg struct {
	wsMsg

	// Session is the client's session
	Session syncSess `json:"session"`
}

// newWsHereisSessMsg initializes a wsHereisSessMsg
func newWsHereisSessMsg() wsHereisSessMsg {
	return wsHereisSessMsg{
		wsMsg: wsMsg{
			Type: "hereis-session",
		},
	}
}

// wsCmdStateMsg contains the server's desired state which a client must try
// to achieve.
type wsCmdStateMsg struct {
	wsMsg

	// State is the server's desired state
	State syncState `json:"state"`
}

// newWsCmdStateMsg initializes a new wsCmdStateMsg
func newWsCmdStateMsg() wsCmdStateMsg {
	return wsCmdStateMsg{
		wsMsg: wsMsg{
			Type: wsCmdStateMsgT,
		},
	}
}

// wsJSON sends a JSON formatted message on a web socket connection.
func (h syncWS) wsJSON(ws *websocket.Conn, data interface{}) {
	if err := ws.WriteJSON(data); err != nil {
		h.log.Errorf("failed to write JSON web socket message: %s", err)
	}
}

// wsErr sends and logs a wsErrMsg message
func (h syncWS) wsErr(ws *websocket.Conn, pubErr, privErr error) {
	errMsg := newWsErrMsg()
	pubErrStr := "nil"
	privErrStr := "nil"

	if pubErr != nil {
		errMsg.Err = pubErr.Error()
		pubErrStr = errMsg.Err
	} else {
		errMsg.Err = "an internal server error occurred"
	}

	if privErr != nil {
		privErrStr = privErr.Error()
	}

	h.wsJSON(ws, errMsg)

	h.log.Errorf("web socket error: public=%s, private=%s", pubErrStr,
		privErrStr)
}

// wsParseJSON parses the data bytes as JSON and stores the results in the
// provided dest interface, who's format is then validated.
//
// If any errors occur an error message is sent to the client via web socket
// and false is returned.
func (h syncWS) wsParseJSON(ws *websocket.Conn, data []byte,
	dest interface{}) bool {
	// Lazy initialize h.validate
	if h.validate == nil {
		h.validate = validator.New()
	}

	// Parse JSON
	if err := json.Unmarshal(data, dest); err != nil {
		h.wsErr(ws, fmt.Errorf("failed to parse message as JSON"), err)
		return false
	}

	// Validate
	if err := h.validate.Struct(dest); err != nil {
		if _, ok := err.(*validator.InvalidValidationError); ok {
			h.wsErr(ws, nil, fmt.Errorf("validator received invalid "+
				"input: %s", err))
		} else if errs, ok := err.(validator.ValidationErrors); ok {
			errStrs := []string{}

			for _, err := range errs {
				errStrs = append(errStrs, fmt.Sprintf("\"%s\" field "+
					"failed the \"%s\" validation", err.Field(),
					err.Tag()))
			}

			h.wsErr(ws, fmt.Errorf("message format invalid: %s",
				strings.Join(errStrs, ", ")), nil)
		} else {
			h.wsErr(ws, nil, fmt.Errorf("unknown message validator "+
				"error: %s", err))
		}

		return false
	}

	return true
}

// ServeHTTP upgrades the request to a web socket and syncs the client with
// the session
func (h syncWS) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	// Manage web socket
	ws, err := h.upgrader.Upgrade(w, r, nil)
	if err != nil {
		h.httpErr(w, 0, fmt.Errorf("failed to upgrade connection to "+
			"web socket"), err)
		return
	}

	defer func() {
		if err := ws.Close(); err != nil {
			h.log.Errorf("failed to close web socket: %s", err)
		}
	}()

	// Loop reading messages
	for {
		// Block read from web socket message
		mt, msgBytes, err := ws.ReadMessage()
		if err != nil {
			if _, ok := err.(*websocket.CloseError); ok {
				// If websocket is closing fail silently and exit loop
				return
			} else {
				// Otherwise log error and loop
				h.log.Errorf("failed to read web socket: %s", err)
				continue
			}
		}

		// Ignore non text messages
		if mt != websocket.TextMessage {
			continue
		}

		var msg wsMsg
		if err := json.Unmarshal(msgBytes, &msg); err != nil {
			h.wsErr(ws, fmt.Errorf("failed to parse web socket message as "+
				"JSON"), err)
			continue
		}

		switch msg.Type {
		case wsCreateSessMsgT:
			// Parse msg as wsCreateSessMsg
			var createMsg wsCreateSessMsg
			if !h.wsParseJSON(ws, msgBytes, &createMsg) {
				continue
			}

			// Make unique ID
			randBuf := make([]byte, 64)
			randUniq := false

			for len(randBuf) == 0 || !randUniq {
				if _, err := rand.Read(randBuf); err != nil {
					h.wsErr(ws, nil,
						fmt.Errorf("failed to generate random: %s", err))
					continue
				}

				keyExists, err := h.db.Exists(syncSessT, string(randBuf))
				if err != nil {
					h.wsErr(ws, nil, fmt.Errorf("failed to check if "+
						"random is unique in db: %s", err))
					return
				}

				randUniq = !keyExists
			}

			// Store in db
			sess := syncSess{
				ID:   string(randBuf),
				Name: createMsg.Name,
			}

			err := h.db.Write(syncSessT, sess.ID, sess)
			if err != nil {
				h.wsErr(ws, nil, fmt.Errorf("failed to store in db: %s",
					err))
				return
			}

			// Send resp
			resp := newWsHereisSessMsg()
			resp.Session = sess
			h.wsJSON(ws, resp)
			break
		default:
			h.wsErr(ws, fmt.Errorf("message type \"%s\" is not valid",
				msg.Type), nil)
			break
		}
	}
}

func main() {
	// Lifetime
	var wg sync.WaitGroup
	ctx, cancelCtx := context.WithCancel(context.Background())
	log := golog.NewStdLogger("video-sync")

	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, os.Interrupt)

	go func() {
		<-sigs
		cancelCtx()
	}()

	// Config
	cfgLdr := goconf.NewDefaultLoader()
	cfgLdr.AddConfigPath("/etc/video-sync/*")
	cfgLdr.AddConfigPath("./*")
	var cfg config
	if err := cfgLdr.Load(&cfg); err != nil {
		log.Fatalf("failed to load config: %s", err)
	}

	// Redis
	redis := redislib.NewClient(&redislib.Options{
		Addr:     cfg.RedisAddr,
		Password: "", // no password set
		DB:       0,  // use default DB
	})

	if _, err := redis.Ping().Result(); err != nil {
		log.Fatalf("failed to connect to redis %s: %s", cfg.RedisAddr, err)
	}

	// HTTP server
	baseH := baseHdlr{
		ctx: ctx,
		log: log,
		db:  redisDatastore{redis},
	}
	router := mux.NewRouter()
	router.Handle("/health", healthHdlr{baseH})
	//router.Handle("/sync", createSyncSession{baseH}).Methods("POST")
	router.Handle("/sync", syncWS{
		baseHdlr: baseH,
		upgrader: websocket.Upgrader{
			CheckOrigin: func(r *http.Request) bool {
				// TODO: Make this safe
				return true
			},
		},
	}).Methods("GET")

	server := http.Server{
		Addr: cfg.APIAddr,
		Handler: logHdlr{
			log:       log,
			childHdlr: router,
		},
	}

	wg.Add(1)
	go func() {
		log.Infof("starting server on %s", server.Addr)

		if err := server.ListenAndServe(); err != nil &&
			err != http.ErrServerClosed {
			log.Fatalf("failed to run server: %s", err)
		}
	}()

	go func() {
		<-ctx.Done()

		log.Info("stopping server")

		if err := server.Shutdown(context.Background()); err != nil {
			log.Fatalf("failed to shutdown server: %s", err)
		}

		wg.Done()
		log.Info("stopped server")
	}()

	wg.Wait()
	log.Info("exiting")
}
