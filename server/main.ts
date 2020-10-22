import { parse as parseToml } from 'https://deno.land/std@0.74.0/encoding/toml.ts';
import { Application, Context } from 'https://deno.land/x/abc@v1.0.3/mod.ts';

// Load configuration
interface Config {
    httpPort: number
}

function isConfig(data: unknown): data is Config {
    if (typeof data !== 'object') {
	   throw 'not an Object';
    } else if (typeof data.httpPort !== 'number') {
	   throw 'httpPort not a number';
    }

    return data;
}

const cfg: Config = isConfig(parseToml(await Deno.readTextFile('./config.toml'))); // TODO: Type cast to Config

// HTTP server
const app = new Application();

app.get('/api/v0/health', (c: Context) => {
    return c.json({
	   ok: true,
    });
});

app.start({
    port: cfg.httpPort,
});
