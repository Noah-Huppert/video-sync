class syncAPI {
    /**
	 * Initializes sync API client, returns a promise which resolves when
	 * the client is setup.
	 */
    constructor() {
	   this.wsCreateSessMsgT = 'create-session';
	   this.wsHereisSessMsgT = 'hereis-session';
	   this.wsErrMsgT = 'error';
	   
	   this.cfg = undefined;
	   
	   return browser.storage.local.get("config")
		  .then((cfg) => {
			 this.cfg = cfg;
			 if (Object.keys(cfg).length == 0) {
				this.cfg = {
				    syncServer: "localhost:5000"
				};
			 }

			 this.syncWS = this.newSyncWS();
			 this.syncWS.addEventListener('message', this.onWSMsg);

			 return new Promise((resolve, reject) => {
				this.syncWS.addEventListener('open', () => {
				    resolve(this);
				});
			 });
		  });
    }

    /**
	 * Handler for messages received by syncWS.
	 * Different logic is called based on the .type field of the message.
      */
    onWSMsg(event) {
	   // Parse message
	   const msg = JSON.parse(event.data);

	   if (!msg.hasOwnProperty('type')) {
		  reportErr("Received invalid message from server",
				  "web socket message received with no \"type\" field, " +
				  "message=" + event.data);
		  return;
	   }

	   // Handle
	   console.log("msg.type", msg.type, this.wsHereisSessMsgT);
	   switch (msg.type) {
	   case this.wsHereisSessMsgT:
		  console.log("setting session");
		  setSyncSess(msg.session)
			 .then(() => {
				console.log("set sess");
			 })
			 .catch((err) => {
				console.error("failed set sess", err);
			 });
		  break;
	   case this.wsErrMsgT:
		  reportErr(msg.error, undefined);
		  break;
	   }
    }

    /**
	* Stores error in local StorageArea so extension can notify user and reports
	* the error to the server.
	*/
    reportErr(pubErr, privErr) {
	   // Log
	   errStr = "public=" + pubErr + "private=" + privErr;
	   console.error("error reported", errStr);

	   // Store in local storage
	   var errs = browser.storage.local.get("errors") || [];
	   errs.append({
		  public: pubErr,
		  private: privErr
	   });
	   browser.storage.local.set("errors", errs);

	   // Report via web socket
	   syncWS.send(JSON.stringify({
		  type: wsErrMsgT,
		  error: errStr
	   }));
    }

    /**
	* Returns a web socket connected to the sync server.
	*/
    newSyncWS() {
	   return new WebSocket("ws://" + this.cfg.syncServer + "/sync");
    }

    /**
	* Returns the current sync session, or null if none exists.
	*/
    getSyncSess() {
	   return browser.storage.local.get("sync_session");
    }


    /**
	* Sets current sync session.
	*/
    setSyncSess(sess) {
	   return browser.storage.local.set("sync_session", sess);
    }

    /**
     * Sends create session message on web socket.
     */
    wsCreateSess(name) {
	   return this.syncWS.send(JSON.stringify({
		  type: this.wsCreateSessMsgT,
		  name: name
	   }));
    }
}
