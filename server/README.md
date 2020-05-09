# Server
Sync server.

# Table Of Contents
- [Overview](#overview)
- [Development](#development)

# Overview
Receives state from client browser extensions and sends commands to 
synchronize them. 

# Development
Rust is used for server programming. Podman is used to run a local Redis server.

First start the local Redis server:

```
./redis start
```

Then run the API server:

```
cargo run
```
