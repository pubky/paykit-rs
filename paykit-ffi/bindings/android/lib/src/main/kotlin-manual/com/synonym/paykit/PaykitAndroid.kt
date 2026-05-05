package com.synonym.paykit

import android.content.Context

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
