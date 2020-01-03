# Server
Sync server.

# Table Of Contents
- [Overview](#overview)
- [Development](#development)

# Overview
Receives state from client browser extensions and sends commands to 
synchronize them. 

# Development
Golang is used.

A custom message protocol is used to communicate with the web extension client,
see the `syncWS` struct docs for details.

Podman is used for local development.

First run a local Redis server:

```
./redis start
```

Then start the server:

```
./run
```

Relaunch `./run` when changes to the code are made.

Stop the Redis server when done:

```
./redis stop
```
