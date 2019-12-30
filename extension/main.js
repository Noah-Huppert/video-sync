// Set CSP
/*
document.head.innerHTML += "<meta http-equiv=\"Content-Security-Policy\" " +
    "content=\"default-src moz-extension https: wss: 'unsafe-inline' 'unsafe-eval'; font-src https: data: ;img-src  https: data: blob: ;media-src https: blob: ;\">";
*/

// Check if joining a room via URL
var query = {}
if (window.location.search.length > 0) {
    var search = window.location.search;
    
    if (search[0] == "?") {
	   search = search.substr(1);
    }

    var searchParts = search.split("&");
    for (var i = 0; i < searchParts.length; i++) {
	   var param = searchParts[i];
	   
	   var parts = param.split("=");
	   var key = parts[0];
	   var value = "";
	   if (parts.length > 1) {
		  value = decodeURIComponent(parts[1]);
	   }
	   query[key] = value;
    }
}

if ("video-sync" in query) {
    localStorage.setItem("videoSyncID", query["video-sync"]);
}

// Get settings from storage
var videoSyncID = localStorage.getItem("videoSyncID");
var videoSyncAPIHost = localStorage.getItem("videoSyncAPIHost") || "localhost:5000";

console.log("videoSyncID", videoSyncID, videoSyncAPIHost);

// Create video sync session if none specified
if (videoSyncID == null) {
    console.log("post /sync commented out");
    /*
    fetch("http://" + videoSyncAPIHost + "/sync", {
	   method: "POST",
	   headers: {
		  "Content-Type": "application/json"
	   }
    }).then(function(resp) {
	   if (!resp.ok) {
		  console.error("failed to create new sync session: " +
					 resp["error"]);
	   } else {
		  console.log("sync session: ", resp);
		  localStorage.setItem("videoSyncID", resp["sync_session"]["id"]);
		  videoSyncID = resp["sync_session"]["id"];
	   }
    }).catch(function(err) {
	   console.error("failed to create sync session", err);
    });
*/
}

// Create a websocket to recieve sync session commands
var syncWS = new WebSocket("ws://" + videoSyncAPIHost + "/sync");
var syncWSReady = false;

syncWS.onopen = function() {
    syncWSReady = true;
};

syncWS.onmessage = function(msg) {
    console.log("syncWS received", msg);
};

// Poll until video element loads and syncWS is open. onMounted is called when
// all pre-conditions are met.
var mountCheckInt = setInterval(function() {
    // Check for videos
    var videos = document.getElementsByTagName("video");
    if (videos.length > 1) {
	   console.error("found more than one video frame: " + videos.length);
	   clearInterval(mountCheckInt);
	   return;
    }

    // Call onMounted if pre-conditions are met
    if (syncWSReady && videos.length == 1) {
	   clearInterval(mountCheckInt);
	   onMounted(videos[0]);
    }
}, 100);

// Called when video element loads
function onMounted(video) {
    syncWS.send("{\"type\": \"create-session\"}");
    console.log("ct", video.currentTime);
    //syncWS.send("hello world");
}
