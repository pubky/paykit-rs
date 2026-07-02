package com.synonym.paykit

import android.content.Context

public object PaykitSdkDefaults {
    @JvmField
    public val DEFAULT_ENDPOINT_MANAGEMENT_SCOPE: EndpointManagementScope =
        EndpointManagementScope.MANAGED_ONLY

    @JvmField
    public val DEFAULT_ENCRYPTED_LINK_RECOVERY_MARKER_POLICY: EncryptedLinkRecoveryMarkerPolicy =
        EncryptedLinkRecoveryMarkerPolicy.ENABLED

    @JvmField
    public val DEFAULT_PUBLIC_CONTACT_SHARING_POLICY: PublicContactSharingPolicy =
        PublicContactSharingPolicy.LOCAL_ONLY
}

public object PaykitAndroid {
    init {
        System.loadLibrary("paykit")
    }

    @JvmStatic
    public fun initialize(context: Context): Boolean =
        nativeInitialize(context.applicationContext)

    @JvmStatic
    public fun initializeOrThrow(context: Context) {
        check(initialize(context)) {
            "failed to initialize Paykit Android platform verifier"
        }
    }

    @JvmStatic
    private external fun nativeInitialize(context: Context): Boolean
}
