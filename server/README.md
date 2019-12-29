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

Podman is used for local development.

First run a local Redis server:

```
./redis
```

Then start the server:

```
./run
```

Relaunch `./run` when changes to the code are made.
