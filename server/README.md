# Server
Sync server.

# Table Of Contents
- [Overview](#overview)
- [Development](#development)
- [Redis](#redis)

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

# Redis
Used as a data store and message bus.

Keys follow the format:

```
<type>:<ID>
```

For types which are children of other types always put the types first:

```
<type>:<sub-type>:<type ID>:<sub-type ID>
```

This pattern allows for join type and exact queries.
