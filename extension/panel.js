var syncClient = new syncAPI();

syncClient.then(async (syncClient) => {
    // Create new sync session if none exists
    var syncSess = await syncClient.getSyncSess();
    if (Object.keys(syncSess).length == 0) {
	   await syncClient.wsCreateSess("My Sync Session");
    }

    console.log("sync rsolve");
});

browser.storage.onChanged.addListener(function(changes, areaName) {
    console.log("storage.local.onChanged, changes=", changes);
});

console.log("heelo");
