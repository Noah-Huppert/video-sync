import {
    Application,
    Context,

    Serializable,
    SerializeProperty,
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
    mongodbURI: string = "mongodb://devvideosync:devvideosync@127.0.0.1:27017/dev-video-sync";
}

const cfg = new Config().fromJSON(JSON.parse(await Deno.readTextFile("./config.json")));

// HTTP server
const app = new Application();

app.get("/api/v0/health", (c: Context) => {
    return c.json({
	   ok: true,
    });
});

console.log(`Starting HTTP API on :${cfg.httpPort}`);
app.start({
    port: cfg.httpPort,
});
