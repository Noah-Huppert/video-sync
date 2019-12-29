package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"time"

	"github.com/Noah-Huppert/goconf"
	"github.com/Noah-Huppert/golog"
	redislib "github.com/go-redis/redis/v7"
	"github.com/gorilla/mux"
	"github.com/gorilla/websocket"
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
		return fmt.Errorf("failed to get data from redis: %s", err)
	}

	if err := json.Unmarshal(result.Val(), data); err != nil {
		return fmt.Errorf("failed to decode data as JSON: %s", err)
	}

	return nil
}

// Exists determines if a key exists
func (rd redisDatastore) Exists(dataType, name string) (bool, error) {
	result := rd.redis.Exists(fmt.Sprintf("%s:%s", dataType, name))
	if result.Err() != nil {
		return fmt.Errorf("failed to query redis for key: %s", err)
	}

	return result.Val() == 1
}

// syncSession
type syncSession struct {
	// ID of session
	ID string `json:"id"`
}

// baseHdlr has useful helpers
type baseHdlr struct {
	ctx context.Context
	log golog.Logger
	db  datastore
}

// defHttpErr
var defHttpErr []byte = []byte("{\"error\": \"internal server error\"}")

// httpError returns a JSON error response and logs.
// Set status to 0 for default response HTTP status
// of http.StatusInternalServerError.
func (h baseHdlr) httpError(w http.ResponseWriter, status int, err,
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
		h.httpError(w, 0, nil, fmt.Errorf("failed to write JSON http "+
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

/*
// createSyncSession creates a new session for a client
type createSyncSession struct {
	baseHdlr
}

// ServeHTTP stores a new syncSession in db and returns to the client in JSON
func (h createSyncSession) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	// Make unique ID
	randBuf := make([]byte, 64)
	randUniq := false

	for len(randBuf) == 0 || !randUniq {
		if _, err := rand.Read(randBuf); err != nil {
			h.httpError(w, 0, nil,
				fmt.Errorf("failed to generate random: %s", err))
			return
		}

		ok, err := h.redis.Exists(fmt.Sprintf("sync:%s", randBuf)).Result()
		if err != nil {
			h.httpError(w, 0, nil,
				fmt.Errorf("failed to check if random is unique in "+
					"redis: %s", err))
			return
		}

		if ok == 0 {
			randUniq = true
		}
	}

	h.log.Debugf("sync: rand=%s", randBuf)

	// Store in db
	s := syncSession{
		ID: string(randBuf),
	}

	err := h.redis.Set(fmt.Sprintf("sync:%s", s.ID), s.ID, 0)
	if err != nil {
		h.httpError(w, 0, nil, fmt.Errorf("failed to store in db: %s",
			err))
		return
	}

	// Send resp
	h.httpJSON(w, map[string]interface{}{
		"sync_session": s,
	})
}
*/

// syncWS upgrades connections to web sockets and uses a custom protocol to
// keep client's videos in sync.
///
// Data is encoded in JSON on the web socket.
// All messages must have a "type" field. Depending on the type additional
// fields may be required. Different message types can only be sent by the
// server or the client.
//
// Types:
//
//    - create-session: (client->server) Create sync session, no additional
//request
//              fields, see wSCreateResp for response
//    - session: (server->client)
//    - report: (client->server) Client report of current video status, see
//              wSReportReq for additional request fields, no response sent
//    - command: (server->client) Request to update video player's state, see
//               wSCommandReq, no response sent
type syncWS struct {
	baseHdlr

	// upgrader is used to upgrade HTTP connections to to web
	// socket connections
	upgrader websocket.Upgrader
}

// wsMsg is the base format which all messages sent and received via the
// web socket must follow.
type wsMsg struct {
	// Type identifies the type of message, see syncWS docs for valid values
	Type wsMsgType `json:"type"`
}

// wsMsgType indicates the type of web socket message, see syncWS docs
// for details
type wsMsgType string

// web socket message types, see syncWS docs for details
const (
	createMsgT  wsMsgType = "create"
	reportMsgT  wsMsgType = "report"
	commandMsgT wsMsgType = "command"
)

// wsCreateResp is the response sent by the server to the client when a new sync
// session is created
type wsCreateResp struct {
	// Session is the created sync session
	Session syncSession `json:"session"`
}

// ServeHTTP upgrades the request to a web socket and syncs the client with
// the session
func (h syncWS) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	/*
		// Check sync session ID exists
		urlVars := mux.Vars(r)
		syncSessionID, ok := urlVars["sync_session_id"]
		if !ok {
			h.httpError(w, http.StatusBadRequest,
				fmt.Errorf("sync session id must be provided in URL"), nil)
			return
		}

		exists, err := h.redis.Exists(fmt.Sprintf("sync:%s", syncSessionID)).
			Result()
		if err != nil {
			h.httpError(w, 0, nil,
				fmt.Errorf("failed to check if sync session ID exists: %s", err))
			return
		}

		if exists == 0 {
			h.httpError(w, http.StatusNotFound, fmt.Errorf("sync session with "+
				"ID \"%s\" does not exist", syncSessionID), nil)
			return
		}
	*/

	// Manage web socket
	ws, err := h.upgrader.Upgrade(w, r, nil)
	if err != nil {
		h.httpError(w, 0, fmt.Errorf("failed to upgrade connection to "+
			"web socket"), err)
		return
	}

	defer func() {
		if err := ws.Close(); err != nil {
			h.log.Errorf("failed to close web socket: %s", err)
		}
	}()

	for {
		// Block read for web socket message
		mt, msg, err := ws.ReadMessage()
		if err != nil {
			// If websocket is closing fail silently
			if _, ok := err.(*websocket.CloseError); !ok {
				h.log.Errorf("failed to read web socket: %s", err)
			}

			return
		}

		// Ignore non text messages
		if mt != websocket.TextMessage {
			continue
		}

		h.log.Debugf("received ws message: %s", msg)
		// TODO: lUnseralize message to wsMsg then handle by .Type field

		// Echo write back
		if err := ws.WriteMessage(websocket.TextMessage, msg); err != nil {
			h.log.Errorf("failed to write web socket: %s", err)
			return
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

	// Regis
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
		ctx:   ctx,
		log:   log,
		redis: redis,
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
