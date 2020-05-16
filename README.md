# Video Sync
Synchronize videos between friends to create a remote theater experience.

# Table Of Contents
- [Overview](#overview)
- [Design](#design)

# Overview
Synchronizes a video across devices.

See [`extension/`](./extension) for web extension code.  
See [`server/`](./server) for sync server code.

# Design
Users join sync sessions. The video state is synchronized inside this session.

A HTTP API is used to set state. A web socket is used to push notifications of
state updates.
