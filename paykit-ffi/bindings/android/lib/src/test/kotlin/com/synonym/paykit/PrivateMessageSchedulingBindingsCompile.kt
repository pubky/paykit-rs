package com.synonym.paykit

// Compile-only consumer fixture for private message scheduling.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compilePrivateMessageSchedulingBindingsSurface(sdk: PaykitSdkInterface) {
    val snapshot: PrivateMessageSchedulingSnapshot = sdk.privateMessageSchedulingSnapshot()
    val peers: List<LinkedPeerRecord> = snapshot.linkedPeers
    val pending: List<String> = snapshot.pendingOutboundCounterparties
}
