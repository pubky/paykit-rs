package com.synonym.paykit

import android.content.Context

public object PaykitSdkDefaults {
    @JvmField
    public val DEFAULT_ENDPOINT_MANAGEMENT_SCOPE: FfiEndpointManagementScope =
        FfiEndpointManagementScope.MANAGED_ONLY

    @JvmField
    public val DEFAULT_ENCRYPTED_LINK_RECOVERY_MARKER_POLICY: FfiEncryptedLinkRecoveryMarkerPolicy =
        FfiEncryptedLinkRecoveryMarkerPolicy.ENABLED

    @JvmField
    public val DEFAULT_PUBLIC_CONTACT_SHARING_POLICY: FfiPublicContactSharingPolicy =
        FfiPublicContactSharingPolicy.LOCAL_ONLY
}

public object PaykitAndroid {
    init {
        System.loadLibrary("paykit")
    }

    @JvmStatic
    public fun initialize(context: Context): Boolean =
        nativeInitialize(context.applicationContext)

    @JvmStatic
    private external fun nativeInitialize(context: Context): Boolean
}
