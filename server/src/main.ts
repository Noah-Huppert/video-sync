import {
    Application,
    Context,

    Serializable,
    SerializeProperty,

    Bson,
    MongoClient,
} from "./deps.ts";

/**
 * HTTP API server configuration.
 */
class Config extends Serializable {
    /**
	* Port on which to listen for HTTP API connections.
	*/
    @SerializeProperty()
    httpPort: number = 8000;

    /**
	* MongoDB connection URI.
	*/
    @SerializeProperty()
    mongoURI: string = "mongodb://devvideosync:devvideosync@127.0.0.1:27017/";

    /**
	* Name MongoDB database in which to store data.
	*/
    @SerializeProperty()
    mongoDBName = "dev-video-sync";
}

/**
 * API server.
 */
class API {
    /**
	* API configuration.
	*/
    cfg: Config;

    /**
	* MongoDB Client.
	*/
    mongo: MongoClient;

    /**
	* Initializes an API.
	*/
    constructor(cfg: Config, mongo: MongoClient) {
	   this.cfg = cfg;
	   this.mongo = mongo;
    }

    /**
	* Loads configuration, connects to the database and uses to make a new API.
	*/
    static async MakeAPI() {
	   // Load configuration
	   const cfg = new Config().fromJSON(JSON.parse(await Deno.readTextFile("./config.json")));
	   console.log("Loaded configuration");

	   // Connect to MongoDB
	   const mongo = new MongoClient();
	   await mongo.connect(cfg.mongoURI);
	   console.log("Connected to MongoDB");

	   return new API(cfg, mongo);
    }

    /**
	* Starts the HTTP API server.
	*/
    httpListen() {
	   const app = new Application();

	   // Endpoints
	   app.get("/api/v0/health", this.ept_health);

	   // Start
	   console.log(`Starting HTTP API on :${this.cfg.httpPort}`);
	   app.start({
		  port: this.cfg.httpPort,
	   });
    }

    /**
	* Health check endpoint.
	* Request: N/A
	* Response: HealthCheckResp
	*/
    ept_health(c: Context) {
	   return c.json(new HealthCheckResp(true).toJSON());
    }
}

/**
 * Health check endpoint response.
 */
class HealthCheckResp extends Serializable {
    /**
	* Indicates if the API is functioning.
	*/
    @SerializeProperty()
    ok: boolean;

    constructor(ok: boolean) {
	   super();
	   this.ok = ok;
    }
}



// Start API
let api = await API.MakeAPI();
api.httpListen();
