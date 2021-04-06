# Server
Sync server.

# Table Of Contents
- [Overview](#overview)
- [Development](#development)

# Overview
Receives state from client browser extensions and sends commands to synchronize them. 

# Development
[Deno JS](https://deno.land) is used.

Start a MongoDB container:

```sh
docker-compose up -d
```

Start the server:

```sh
make
```
