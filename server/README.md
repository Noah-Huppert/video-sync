# Server
Sync server.

# Table Of Contents
- [Overview](#overview)
- [Development](#development)

# Overview
Receives state from client browser extensions and sends commands to 
synchronize them. 

# Development
Rust is used.

Podman is used for local development.

First run a local Redis server:

```
./redis start
```

Then run the server:

```
cargo run
```
