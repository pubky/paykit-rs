

@file:Suppress("RemoveRedundantBackticks")

package com.synonym.paykit

// Common helper code.
//
// Ideally this would live in a separate .kt file where it can be unittested etc
// in isolation, and perhaps even published as a re-useable package.
//
// However, it's important that the details of how this helper code works (e.g. the
// way that different builtin types are passed across the FFI) exactly match what's
// expected by the Rust code on the other side of the interface. In practice right
// now that means coming from the exact some version of `uniffi` that was used to
// compile the Rust component. The easiest way to ensure this is to bundle the Kotlin
// helpers directly inline like we're doing here.

import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.Structure
import android.os.Build
import androidx.annotation.RequiresApi
import kotlin.coroutines.resume
import kotlinx.coroutines.CancellableContinuation
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext


internal typealias Pointer = com.sun.jna.Pointer
internal val NullPointer: Pointer? = com.sun.jna.Pointer.NULL
internal fun Pointer.toLong(): Long = Pointer.nativeValue(this)
internal fun kotlin.Long.toPointer() = com.sun.jna.Pointer(this)


@kotlin.jvm.JvmInline
public value class ByteBuffer(private val inner: java.nio.ByteBuffer) {
    init {
        inner.order(java.nio.ByteOrder.BIG_ENDIAN)
    }

    public fun internal(): java.nio.ByteBuffer = inner

    public fun limit(): Int = inner.limit()

    public fun position(): Int = inner.position()

    public fun hasRemaining(): Boolean = inner.hasRemaining()

    public fun get(): Byte = inner.get()

    public fun get(bytesToRead: Int): ByteArray = ByteArray(bytesToRead).apply(inner::get)

    public fun getShort(): Short = inner.getShort()

    public fun getInt(): Int = inner.getInt()

    public fun getLong(): Long = inner.getLong()

    public fun getFloat(): Float = inner.getFloat()

    public fun getDouble(): Double = inner.getDouble()

    public fun put(value: Byte) {
        inner.put(value)
    }

    public fun put(src: ByteArray) {
        inner.put(src)
    }

    public fun putShort(value: Short) {
        inner.putShort(value)
    }

    public fun putInt(value: Int) {
        inner.putInt(value)
    }

    public fun putLong(value: Long) {
        inner.putLong(value)
    }

    public fun putFloat(value: Float) {
        inner.putFloat(value)
    }

    public fun putDouble(value: Double) {
        inner.putDouble(value)
    }
}
public fun RustBuffer.setValue(array: RustBufferByValue) {
    this.data = array.data
    this.len = array.len
    this.capacity = array.capacity
}

internal object RustBufferHelper {
    internal fun allocValue(size: ULong = 0UL): RustBufferByValue = uniffiRustCall { status ->
        // Note: need to convert the size to a `Long` value to make this work with JVM.
        UniffiLib.ffi_paykit_rustbuffer_alloc(size.toLong(), status)
    }.also {
        if(it.data == null) {
            throw RuntimeException("RustBuffer.alloc() returned null data pointer (size=${size})")
        }
    }

    internal fun free(buf: RustBufferByValue) = uniffiRustCall { status ->
        UniffiLib.ffi_paykit_rustbuffer_free(buf, status)
    }
}

@Structure.FieldOrder("capacity", "len", "data")
public open class RustBufferStruct(
    // Note: `capacity` and `len` are actually `ULong` values, but JVM only supports signed values.
    // When dealing with these fields, make sure to call `toULong()`.
    @JvmField public var capacity: Long,
    @JvmField public var len: Long,
    @JvmField public var data: Pointer?,
) : Structure() {
    public constructor(): this(0.toLong(), 0.toLong(), null)

    public class ByValue(
        capacity: Long,
        len: Long,
        data: Pointer?,
    ): RustBuffer(capacity, len, data), Structure.ByValue {
        public constructor(): this(0.toLong(), 0.toLong(), null)
    }

    /**
     * The equivalent of the `*mut RustBuffer` type.
     * Required for callbacks taking in an out pointer.
     *
     * Size is the sum of all values in the struct.
     */
    public class ByReference(
        capacity: Long,
        len: Long,
        data: Pointer?,
    ): RustBuffer(capacity, len, data), Structure.ByReference {
        public constructor(): this(0.toLong(), 0.toLong(), null)
    }
}

public typealias RustBuffer = RustBufferStruct
public typealias RustBufferByValue = RustBufferStruct.ByValue

internal fun RustBuffer.asByteBuffer(): ByteBuffer? {
    require(this.len <= Int.MAX_VALUE) {
        val length = this.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
    return ByteBuffer(data?.getByteBuffer(0L, this.len) ?: return null)
}

internal fun RustBufferByValue.asByteBuffer(): ByteBuffer? {
    require(this.len <= Int.MAX_VALUE) {
        val length = this.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
    return ByteBuffer(data?.getByteBuffer(0L, this.len) ?: return null)
}

// This is a helper for safely passing byte references into the rust code.
// It's not actually used at the moment, because there aren't many things that you
// can take a direct pointer to in the JVM, and if we're going to copy something
// then we might as well copy it into a `RustBuffer`. But it's here for API
// completeness.

@Structure.FieldOrder("len", "data")
internal open class ForeignBytesStruct : Structure() {
    @JvmField var len: Int = 0
    @JvmField var data: Pointer? = null

    internal class ByValue : ForeignBytes(), Structure.ByValue
}
internal typealias ForeignBytes = ForeignBytesStruct
internal typealias ForeignBytesByValue = ForeignBytesStruct.ByValue

public interface FfiConverter<KotlinType, FfiType> {
    // Convert an FFI type to a Kotlin type
    public fun lift(value: FfiType): KotlinType

    // Convert an Kotlin type to an FFI type
    public fun lower(value: KotlinType): FfiType

    // Read a Kotlin type from a `ByteBuffer`
    public fun read(buf: ByteBuffer): KotlinType

    // Calculate bytes to allocate when creating a `RustBuffer`
    //
    // This must return at least as many bytes as the write() function will
    // write. It can return more bytes than needed, for example when writing
    // Strings we can't know the exact bytes needed until we the UTF-8
    // encoding, so we pessimistically allocate the largest size possible (3
    // bytes per codepoint).  Allocating extra bytes is not really a big deal
    // because the `RustBuffer` is short-lived.
    public fun allocationSize(value: KotlinType): ULong

    // Write a Kotlin type to a `ByteBuffer`
    public fun write(value: KotlinType, buf: ByteBuffer)

    // Lower a value into a `RustBuffer`
    //
    // This method lowers a value into a `RustBuffer` rather than the normal
    // FfiType.  It's used by the callback interface code.  Callback interface
    // returns are always serialized into a `RustBuffer` regardless of their
    // normal FFI type.
    public fun lowerIntoRustBuffer(value: KotlinType): RustBufferByValue {
        val rbuf = RustBufferHelper.allocValue(allocationSize(value))
        val bbuf = rbuf.asByteBuffer()!!
        write(value, bbuf)
        return RustBufferByValue(
            capacity = rbuf.capacity,
            len = bbuf.position().toLong(),
            data = rbuf.data,
        )
    }

    // Lift a value from a `RustBuffer`.
    //
    // This here mostly because of the symmetry with `lowerIntoRustBuffer()`.
    // It's currently only used by the `FfiConverterRustBuffer` class below.
    public fun liftFromRustBuffer(rbuf: RustBufferByValue): KotlinType {
        val byteBuf = rbuf.asByteBuffer()!!
        try {
           val item = read(byteBuf)
           if (byteBuf.hasRemaining()) {
               throw RuntimeException("junk remaining in buffer after lifting, something is very wrong!!")
           }
           return item
        } finally {
            RustBufferHelper.free(rbuf)
        }
    }
}

// FfiConverter that uses `RustBuffer` as the FfiType
public interface FfiConverterRustBuffer<KotlinType>: FfiConverter<KotlinType, RustBufferByValue> {
    override fun lift(value: RustBufferByValue): KotlinType = liftFromRustBuffer(value)
    override fun lower(value: KotlinType): RustBufferByValue = lowerIntoRustBuffer(value)
}

internal const val UNIFFI_CALL_SUCCESS = 0.toByte()
internal const val UNIFFI_CALL_ERROR = 1.toByte()
internal const val UNIFFI_CALL_UNEXPECTED_ERROR = 2.toByte()

// Default Implementations
internal fun UniffiRustCallStatus.isSuccess(): Boolean
    = code == UNIFFI_CALL_SUCCESS

internal fun UniffiRustCallStatus.isError(): Boolean
    = code == UNIFFI_CALL_ERROR

internal fun UniffiRustCallStatus.isPanic(): Boolean
    = code == UNIFFI_CALL_UNEXPECTED_ERROR

internal fun UniffiRustCallStatusByValue.isSuccess(): Boolean
    = code == UNIFFI_CALL_SUCCESS

internal fun UniffiRustCallStatusByValue.isError(): Boolean
    = code == UNIFFI_CALL_ERROR

internal fun UniffiRustCallStatusByValue.isPanic(): Boolean
    = code == UNIFFI_CALL_UNEXPECTED_ERROR

// Each top-level error class has a companion object that can lift the error from the call status's rust buffer
public interface UniffiRustCallStatusErrorHandler<E> {
    public fun lift(errorBuf: RustBufferByValue): E
}

// Helpers for calling Rust
// In practice we usually need to be synchronized to call this safely, so it doesn't
// synchronize itself

// Call a rust function that returns a Result<>.  Pass in the Error class companion that corresponds to the Err
internal inline fun <U, E: kotlin.Exception> uniffiRustCallWithError(errorHandler: UniffiRustCallStatusErrorHandler<E>, crossinline callback: (UniffiRustCallStatus) -> U): U {
    return UniffiRustCallStatusHelper.withReference() { status ->
        val returnValue = callback(status)
        uniffiCheckCallStatus(errorHandler, status)
        returnValue
    }
}

// Check `status` and throw an error if the call wasn't successful
internal fun<E: kotlin.Exception> uniffiCheckCallStatus(errorHandler: UniffiRustCallStatusErrorHandler<E>, status: UniffiRustCallStatus) {
    if (status.isSuccess()) {
        return
    } else if (status.isError()) {
        throw errorHandler.lift(status.errorBuf)
    } else if (status.isPanic()) {
        // when the rust code sees a panic, it tries to construct a rustbuffer
        // with the message.  but if that code panics, then it just sends back
        // an empty buffer.
        if (status.errorBuf.len > 0) {
            throw InternalException(FfiConverterString.lift(status.errorBuf))
        } else {
            throw InternalException("Rust panic")
        }
    } else {
        throw InternalException("Unknown rust call status: $status.code")
    }
}

// UniffiRustCallStatusErrorHandler implementation for times when we don't expect a CALL_ERROR
public object UniffiNullRustCallStatusErrorHandler: UniffiRustCallStatusErrorHandler<InternalException> {
    override fun lift(errorBuf: RustBufferByValue): InternalException {
        RustBufferHelper.free(errorBuf)
        return InternalException("Unexpected CALL_ERROR")
    }
}

// Call a rust function that returns a plain value
internal inline fun <U> uniffiRustCall(crossinline callback: (UniffiRustCallStatus) -> U): U {
    return uniffiRustCallWithError(UniffiNullRustCallStatusErrorHandler, callback)
}

internal inline fun<T> uniffiTraitInterfaceCall(
    callStatus: UniffiRustCallStatus,
    makeCall: () -> T,
    writeReturn: (T) -> Unit,
) {
    try {
        writeReturn(makeCall())
    } catch(e: kotlin.Exception) {
        callStatus.code = UNIFFI_CALL_UNEXPECTED_ERROR
        callStatus.errorBuf = FfiConverterString.lower(e.toString())
    }
}

internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallWithError(
    callStatus: UniffiRustCallStatus,
    makeCall: () -> T,
    writeReturn: (T) -> Unit,
    lowerError: (E) -> RustBufferByValue
) {
    try {
        writeReturn(makeCall())
    } catch(e: kotlin.Exception) {
        if (e is E) {
            callStatus.code = UNIFFI_CALL_ERROR
            callStatus.errorBuf = lowerError(e)
        } else {
            callStatus.code = UNIFFI_CALL_UNEXPECTED_ERROR
            callStatus.errorBuf = FfiConverterString.lower(e.toString())
        }
    }
}

@Structure.FieldOrder("code", "errorBuf")
internal open class UniffiRustCallStatusStruct(
    @JvmField public var code: Byte,
    @JvmField public var errorBuf: RustBufferByValue,
) : Structure() {
    internal constructor(): this(0.toByte(), RustBufferByValue())

    internal class ByValue(
        code: Byte,
        errorBuf: RustBufferByValue,
    ): UniffiRustCallStatusStruct(code, errorBuf), Structure.ByValue {
        internal constructor(): this(0.toByte(), RustBufferByValue())
    }
    internal class ByReference(
        code: Byte,
        errorBuf: RustBufferByValue,
    ): UniffiRustCallStatusStruct(code, errorBuf), Structure.ByReference {
        internal constructor(): this(0.toByte(), RustBufferByValue())
    }
}

internal typealias UniffiRustCallStatus = UniffiRustCallStatusStruct.ByReference
internal typealias UniffiRustCallStatusByValue = UniffiRustCallStatusStruct.ByValue

internal object UniffiRustCallStatusHelper {
    internal fun allocValue() = UniffiRustCallStatusByValue()
    internal fun <U> withReference(block: (UniffiRustCallStatus) -> U): U {
        val status = UniffiRustCallStatus()
        return block(status)
    }
}

internal class UniffiHandleMap<T: Any> {
    private val map = java.util.concurrent.ConcurrentHashMap<Long, T>()
    private val counter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    internal val size: Int
        get() = map.size

    // Insert a new object into the handle map and get a handle for it
    internal fun insert(obj: T): Long {
        val handle = counter.getAndAdd(1)
        map[handle] = obj
        return handle
    }

    // Get an object from the handle map
    internal fun get(handle: Long): T {
        return map[handle] ?: throw InternalException("UniffiHandleMap.get: Invalid handle")
    }

    // Remove an entry from the handlemap and get the Kotlin object back
    internal fun remove(handle: Long): T {
        return map.remove(handle) ?: throw InternalException("UniffiHandleMap.remove: Invalid handle")
    }
}

internal typealias ByteByReference = com.sun.jna.ptr.ByteByReference
internal typealias DoubleByReference = com.sun.jna.ptr.DoubleByReference
internal typealias FloatByReference = com.sun.jna.ptr.FloatByReference
internal typealias IntByReference = com.sun.jna.ptr.IntByReference
internal typealias LongByReference = com.sun.jna.ptr.LongByReference
internal typealias PointerByReference = com.sun.jna.ptr.PointerByReference
internal typealias ShortByReference = com.sun.jna.ptr.ShortByReference

// Contains loading, initialization code,
// and the FFI Function declarations in a com.sun.jna.Library.

// Define FFI callback types
internal interface UniffiRustFutureContinuationCallback: com.sun.jna.Callback {
    public fun callback(`data`: Long,`pollResult`: Byte,)
}
internal interface UniffiForeignFutureFree: com.sun.jna.Callback {
    public fun callback(`handle`: Long,)
}
internal interface UniffiCallbackInterfaceFree: com.sun.jna.Callback {
    public fun callback(`handle`: Long,)
}
@Structure.FieldOrder("handle", "free")
internal open class UniffiForeignFutureStruct(
    @JvmField public var `handle`: Long,
    @JvmField public var `free`: UniffiForeignFutureFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `handle` = 0.toLong(),

        `free` = null,

    )

    internal class UniffiByValue(
        `handle`: Long,
        `free`: UniffiForeignFutureFree?,
    ): UniffiForeignFuture(`handle`,`free`,), Structure.ByValue
}

internal typealias UniffiForeignFuture = UniffiForeignFutureStruct

internal fun UniffiForeignFuture.uniffiSetValue(other: UniffiForeignFuture) {
    `handle` = other.`handle`
    `free` = other.`free`
}
internal fun UniffiForeignFuture.uniffiSetValue(other: UniffiForeignFutureUniffiByValue) {
    `handle` = other.`handle`
    `free` = other.`free`
}

internal typealias UniffiForeignFutureUniffiByValue = UniffiForeignFutureStruct.UniffiByValue
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU8Struct(
    @JvmField public var `returnValue`: Byte,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toByte(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Byte,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU8(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU8 = UniffiForeignFutureStructU8Struct

internal fun UniffiForeignFutureStructU8.uniffiSetValue(other: UniffiForeignFutureStructU8) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU8.uniffiSetValue(other: UniffiForeignFutureStructU8UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU8UniffiByValue = UniffiForeignFutureStructU8Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU8: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU8UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI8Struct(
    @JvmField public var `returnValue`: Byte,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toByte(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Byte,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI8(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI8 = UniffiForeignFutureStructI8Struct

internal fun UniffiForeignFutureStructI8.uniffiSetValue(other: UniffiForeignFutureStructI8) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI8.uniffiSetValue(other: UniffiForeignFutureStructI8UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI8UniffiByValue = UniffiForeignFutureStructI8Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI8: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI8UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU16Struct(
    @JvmField public var `returnValue`: Short,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toShort(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Short,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU16(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU16 = UniffiForeignFutureStructU16Struct

internal fun UniffiForeignFutureStructU16.uniffiSetValue(other: UniffiForeignFutureStructU16) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU16.uniffiSetValue(other: UniffiForeignFutureStructU16UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU16UniffiByValue = UniffiForeignFutureStructU16Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU16: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU16UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI16Struct(
    @JvmField public var `returnValue`: Short,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toShort(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Short,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI16(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI16 = UniffiForeignFutureStructI16Struct

internal fun UniffiForeignFutureStructI16.uniffiSetValue(other: UniffiForeignFutureStructI16) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI16.uniffiSetValue(other: UniffiForeignFutureStructI16UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI16UniffiByValue = UniffiForeignFutureStructI16Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI16: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI16UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU32Struct(
    @JvmField public var `returnValue`: Int,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0,

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Int,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU32 = UniffiForeignFutureStructU32Struct

internal fun UniffiForeignFutureStructU32.uniffiSetValue(other: UniffiForeignFutureStructU32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU32.uniffiSetValue(other: UniffiForeignFutureStructU32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU32UniffiByValue = UniffiForeignFutureStructU32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU32: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI32Struct(
    @JvmField public var `returnValue`: Int,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0,

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Int,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI32 = UniffiForeignFutureStructI32Struct

internal fun UniffiForeignFutureStructI32.uniffiSetValue(other: UniffiForeignFutureStructI32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI32.uniffiSetValue(other: UniffiForeignFutureStructI32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI32UniffiByValue = UniffiForeignFutureStructI32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI32: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU64Struct(
    @JvmField public var `returnValue`: Long,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toLong(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Long,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU64 = UniffiForeignFutureStructU64Struct

internal fun UniffiForeignFutureStructU64.uniffiSetValue(other: UniffiForeignFutureStructU64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU64.uniffiSetValue(other: UniffiForeignFutureStructU64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU64UniffiByValue = UniffiForeignFutureStructU64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU64: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI64Struct(
    @JvmField public var `returnValue`: Long,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.toLong(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Long,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI64 = UniffiForeignFutureStructI64Struct

internal fun UniffiForeignFutureStructI64.uniffiSetValue(other: UniffiForeignFutureStructI64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI64.uniffiSetValue(other: UniffiForeignFutureStructI64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI64UniffiByValue = UniffiForeignFutureStructI64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI64: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructF32Struct(
    @JvmField public var `returnValue`: Float,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.0f,

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Float,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructF32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructF32 = UniffiForeignFutureStructF32Struct

internal fun UniffiForeignFutureStructF32.uniffiSetValue(other: UniffiForeignFutureStructF32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructF32.uniffiSetValue(other: UniffiForeignFutureStructF32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructF32UniffiByValue = UniffiForeignFutureStructF32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteF32: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructF32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructF64Struct(
    @JvmField public var `returnValue`: Double,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = 0.0,

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Double,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructF64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructF64 = UniffiForeignFutureStructF64Struct

internal fun UniffiForeignFutureStructF64.uniffiSetValue(other: UniffiForeignFutureStructF64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructF64.uniffiSetValue(other: UniffiForeignFutureStructF64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructF64UniffiByValue = UniffiForeignFutureStructF64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteF64: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructF64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructPointerStruct(
    @JvmField public var `returnValue`: Pointer?,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = NullPointer,

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: Pointer?,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructPointer(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructPointer = UniffiForeignFutureStructPointerStruct

internal fun UniffiForeignFutureStructPointer.uniffiSetValue(other: UniffiForeignFutureStructPointer) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructPointer.uniffiSetValue(other: UniffiForeignFutureStructPointerUniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructPointerUniffiByValue = UniffiForeignFutureStructPointerStruct.UniffiByValue
internal interface UniffiForeignFutureCompletePointer: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructPointerUniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructRustBufferStruct(
    @JvmField public var `returnValue`: RustBufferByValue,
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `returnValue` = RustBufferHelper.allocValue(),

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `returnValue`: RustBufferByValue,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructRustBuffer(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructRustBuffer = UniffiForeignFutureStructRustBufferStruct

internal fun UniffiForeignFutureStructRustBuffer.uniffiSetValue(other: UniffiForeignFutureStructRustBuffer) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructRustBuffer.uniffiSetValue(other: UniffiForeignFutureStructRustBufferUniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructRustBufferUniffiByValue = UniffiForeignFutureStructRustBufferStruct.UniffiByValue
internal interface UniffiForeignFutureCompleteRustBuffer: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructRustBufferUniffiByValue,)
}
@Structure.FieldOrder("callStatus")
internal open class UniffiForeignFutureStructVoidStruct(
    @JvmField public var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `callStatus` = UniffiRustCallStatusHelper.allocValue(),

    )

    internal class UniffiByValue(
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructVoid(`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructVoid = UniffiForeignFutureStructVoidStruct

internal fun UniffiForeignFutureStructVoid.uniffiSetValue(other: UniffiForeignFutureStructVoid) {
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructVoid.uniffiSetValue(other: UniffiForeignFutureStructVoidUniffiByValue) {
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructVoidUniffiByValue = UniffiForeignFutureStructVoidStruct.UniffiByValue
internal interface UniffiForeignFutureCompleteVoid: com.sun.jna.Callback {
    public fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructVoidUniffiByValue,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`scope`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`counterparty`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`cancellation`: RustBufferByValue,`uniffiOutReturn`: Pointer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`request`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`endpoint`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod0: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod1: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`uniffiOutReturn`: ByteByReference,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod2: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`uniffiOutReturn`: Pointer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`blob`: Pointer?,`expectedRevision`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod2: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`expectedRevision`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
@Structure.FieldOrder("currentReceivingDetails", "reserveReceivingDetails", "cancelReceivingDetailReservation", "selectPaymentEndpointIds", "buildPaymentTarget", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterStruct(
    @JvmField public var `currentReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0?,
    @JvmField public var `reserveReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1?,
    @JvmField public var `cancelReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2?,
    @JvmField public var `selectPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3?,
    @JvmField public var `buildPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4?,
    @JvmField public var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `currentReceivingDetails` = null,

        `reserveReceivingDetails` = null,

        `cancelReceivingDetailReservation` = null,

        `selectPaymentEndpointIds` = null,

        `buildPaymentTarget` = null,

        `uniffiFree` = null,

    )

    internal class UniffiByValue(
        `currentReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0?,
        `reserveReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1?,
        `cancelReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2?,
        `selectPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3?,
        `buildPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter(`currentReceivingDetails`,`reserveReceivingDetails`,`cancelReceivingDetailReservation`,`selectPaymentEndpointIds`,`buildPaymentTarget`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter = UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterStruct

internal fun UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter) {
    `currentReceivingDetails` = other.`currentReceivingDetails`
    `reserveReceivingDetails` = other.`reserveReceivingDetails`
    `cancelReceivingDetailReservation` = other.`cancelReceivingDetailReservation`
    `selectPaymentEndpointIds` = other.`selectPaymentEndpointIds`
    `buildPaymentTarget` = other.`buildPaymentTarget`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterUniffiByValue) {
    `currentReceivingDetails` = other.`currentReceivingDetails`
    `reserveReceivingDetails` = other.`reserveReceivingDetails`
    `cancelReceivingDetailReservation` = other.`cancelReceivingDetailReservation`
    `selectPaymentEndpointIds` = other.`selectPaymentEndpointIds`
    `buildPaymentTarget` = other.`buildPaymentTarget`
    `uniffiFree` = other.`uniffiFree`
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterUniffiByValue = UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterStruct.UniffiByValue
@Structure.FieldOrder("loadSessionAccess", "publicStorageAvailable", "clearSessionAccess", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceFfiSdkPubkySessionProviderStruct(
    @JvmField public var `loadSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod0?,
    @JvmField public var `publicStorageAvailable`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod1?,
    @JvmField public var `clearSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod2?,
    @JvmField public var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `loadSessionAccess` = null,

        `publicStorageAvailable` = null,

        `clearSessionAccess` = null,

        `uniffiFree` = null,

    )

    internal class UniffiByValue(
        `loadSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod0?,
        `publicStorageAvailable`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod1?,
        `clearSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod2?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider(`loadSessionAccess`,`publicStorageAvailable`,`clearSessionAccess`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider = UniffiVTableCallbackInterfaceFfiSdkPubkySessionProviderStruct

internal fun UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider) {
    `loadSessionAccess` = other.`loadSessionAccess`
    `publicStorageAvailable` = other.`publicStorageAvailable`
    `clearSessionAccess` = other.`clearSessionAccess`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPubkySessionProviderUniffiByValue) {
    `loadSessionAccess` = other.`loadSessionAccess`
    `publicStorageAvailable` = other.`publicStorageAvailable`
    `clearSessionAccess` = other.`clearSessionAccess`
    `uniffiFree` = other.`uniffiFree`
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkPubkySessionProviderUniffiByValue = UniffiVTableCallbackInterfaceFfiSdkPubkySessionProviderStruct.UniffiByValue
@Structure.FieldOrder("loadStateBlob", "saveStateBlobAtomically", "clearStateBlob", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreStruct(
    @JvmField public var `loadStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0?,
    @JvmField public var `saveStateBlobAtomically`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1?,
    @JvmField public var `clearStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod2?,
    @JvmField public var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `loadStateBlob` = null,

        `saveStateBlobAtomically` = null,

        `clearStateBlob` = null,

        `uniffiFree` = null,

    )

    internal class UniffiByValue(
        `loadStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0?,
        `saveStateBlobAtomically`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1?,
        `clearStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod2?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceFfiSdkStateBlobStore(`loadStateBlob`,`saveStateBlobAtomically`,`clearStateBlob`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkStateBlobStore = UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreStruct

internal fun UniffiVTableCallbackInterfaceFfiSdkStateBlobStore.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkStateBlobStore) {
    `loadStateBlob` = other.`loadStateBlob`
    `saveStateBlobAtomically` = other.`saveStateBlobAtomically`
    `clearStateBlob` = other.`clearStateBlob`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceFfiSdkStateBlobStore.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreUniffiByValue) {
    `loadStateBlob` = other.`loadStateBlob`
    `saveStateBlobAtomically` = other.`saveStateBlobAtomically`
    `clearStateBlob` = other.`clearStateBlob`
    `uniffiFree` = other.`uniffiFree`
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreUniffiByValue = UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreStruct.UniffiByValue



















































































































































































































































































@Synchronized
private fun findLibraryName(componentName: String): String {
    val libOverride = System.getProperty("uniffi.component.$componentName.libraryOverride")
    if (libOverride != null) {
        return libOverride
    }
    return "paykit"
}

// For large crates we prevent `MethodTooLargeException` (see #2340)
// N.B. the name of the extension is very misleading, since it is
// rather `InterfaceTooLargeException`, caused by too many methods
// in the interface for large crates.
//
// By splitting the otherwise huge interface into two parts
// * UniffiLib
// * IntegrityCheckingUniffiLib (this)
// we allow for ~2x as many methods in the UniffiLib interface.
//
// The `ffi_uniffi_contract_version` method and all checksum methods are put
// into `IntegrityCheckingUniffiLib` and these methods are called only once,
// when the library is loaded.
internal object IntegrityCheckingUniffiLib : Library {
    init {
        Native.register(IntegrityCheckingUniffiLib::class.java, findLibraryName("paykit"))
        uniffiCheckContractApiVersion()
        uniffiCheckApiChecksums()
    }

    private fun uniffiCheckContractApiVersion() {
        // Get the bindings contract version from our ComponentInterface
        val bindingsContractVersion = 29
        // Get the scaffolding contract version by calling the into the dylib
        val scaffoldingContractVersion = ffi_paykit_uniffi_contract_version()
        if (bindingsContractVersion != scaffoldingContractVersion) {
            throw RuntimeException("UniFFI contract version mismatch: try cleaning and rebuilding your project")
        }
    }
    private fun uniffiCheckApiChecksums() {
        if (uniffi_paykit_checksum_func_core_session_capabilities() != 53661.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_default_config() != 40487.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_default_pubky_client_config() != 12841.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_derive_pubky_secret_key() != 37697.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_parse_pubky_auth_url() != 567.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_parse_pubky_resource() != 2298.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_pubky_public_key_from_secret() != 41462.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_required_session_capabilities() != 62729.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_resolve_pubky_url() != 12085.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_accept_link_with_peer() != 32868.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_advance_link_handshake() != 20770.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_block_peer() != 3462.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_config() != 29410.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_contact_record() != 48991.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_contact_records() != 49216.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_current_private_payment_list() != 28155.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_blob() != 43993.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_encrypted_link_recovery_marker_status() != 21009.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list() != 42080.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_state() != 29122.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_paykit_profile() != 57253.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_file() != 313.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_follows() != 44041.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_profile() != 60331.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_text() != 17257.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_identity_status() != 8559.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_initialize() != 60774.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_initiate_link_with_peer() != 54115.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_linked_peers() != 57246.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_observe_encrypted_link_recovery_marker() != 51945.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_pending_outbound_private_counterparties() != 36875.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_process_outbound_private_messages() != 52525.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_process_pending_private_messages() != 56244.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_encrypted_link_recovery_marker() != 29039.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_blob() != 48358.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_profile() != 19918.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_public_contact() != 49322.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages() != 45996.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages_from_linked_peers() != 15229.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_refresh_contact_paykit_profile() != 29974.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_contact() != 19304.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_encrypted_link_recovery_marker() != 10086.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_public_contact() != 46208.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_payment() != 23408.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_profile() != 56264.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_state() != 30409.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_save_contact() != 7511.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sign_out() != 28715.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_contact_markers() != 39954.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints() != 41929.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_unblock_peer() != 22658.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaymentpayload_export_text() != 53824.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiprivateoperationerror_category() != 32940.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiprivateoperationerror_code() != 52491.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiprivateoperationerror_export_debug_details() != 57660.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiprivateoperationerror_redacted_context() != 46174.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkyauthrequest_authorization_url() != 7484.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkyauthrequest_complete() != 26526.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkylocalsecretkey_export_bytes() != 58726.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionaccess_export_local_secret_key() != 61849.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionaccess_export_session_secret() != 4660.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth() != 21644.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_import_session() != 19676.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_resume_auth() != 48603.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_in() != 58947.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_up() != 31163.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_in_auth() != 7015.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_up_auth() != 3775.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffireservationattribution_export_fields() != 11904.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkbackupblob_export_bytes() != 43352.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_receiving_details() != 10401.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_reserve_receiving_details() != 26808.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_cancel_receiving_detail_reservation() != 52453.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_payment_endpoint_ids() != 38997.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_payment_target() != 25000.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_load_session_access() != 52803.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_public_storage_available() != 360.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_clear_session_access() != 38150.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkstateblob_export_bytes() != 31016.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkstateblobstore_load_state_blob() != 17391.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkstateblobstore_save_state_blob_atomically() != 4172.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkstateblobstore_clear_state_blob() != 747.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipaykitsdk_new() != 15447.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipaykitsdk_with_payment_adapter() != 26121.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipaykitsdk_with_payment_adapter_and_pubky_client_config() != 36484.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipaykitsdk_with_pubky_client_config() != 13764.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipaymentpayload_new() != 12481.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkylocalsecretkey_new() != 13295.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionaccess_new() != 2417.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_new() != 44998.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_with_pubky_client_config() != 35417.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffireservationattribution_new() != 43638.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffisdkbackupblob_new() != 36734.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffisdkstateblob_new() != 33848.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
    }

    // Integrity check functions only
    @JvmStatic
    external fun uniffi_paykit_checksum_func_core_session_capabilities(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_default_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_default_pubky_client_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_derive_pubky_secret_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_parse_pubky_auth_url(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_parse_pubky_resource(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_pubky_public_key_from_secret(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_required_session_capabilities(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_resolve_pubky_url(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_accept_link_with_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_advance_link_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_block_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_contact_record(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_contact_records(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_current_private_payment_list(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_blob(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_state(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_fetch_paykit_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_file(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_follows(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_fetch_pubky_text(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_identity_status(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_initialize(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_initiate_link_with_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_linked_peers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_pending_outbound_private_counterparties(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_process_outbound_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_process_pending_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_encrypted_link_recovery_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_blob(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_public_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages_from_linked_peers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_refresh_contact_paykit_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_encrypted_link_recovery_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_public_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_payment(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_state(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_save_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sign_out(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_contact_markers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_unblock_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaymentpayload_export_text(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiprivateoperationerror_category(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiprivateoperationerror_code(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiprivateoperationerror_export_debug_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiprivateoperationerror_redacted_context(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkyauthrequest_authorization_url(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkyauthrequest_complete(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkylocalsecretkey_export_bytes(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionaccess_export_local_secret_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionaccess_export_session_secret(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_import_session(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_resume_auth(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_in(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_up(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_in_auth(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_up_auth(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffireservationattribution_export_fields(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkbackupblob_export_bytes(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_reserve_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_cancel_receiving_detail_reservation(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_payment_endpoint_ids(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_payment_target(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_load_session_access(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_public_storage_available(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpubkysessionprovider_clear_session_access(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkstateblob_export_bytes(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkstateblobstore_load_state_blob(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkstateblobstore_save_state_blob_atomically(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkstateblobstore_clear_state_blob(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipaykitsdk_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipaykitsdk_with_payment_adapter(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipaykitsdk_with_payment_adapter_and_pubky_client_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipaykitsdk_with_pubky_client_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipaymentpayload_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipubkylocalsecretkey_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipubkysessionaccess_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_with_pubky_client_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffireservationattribution_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffisdkbackupblob_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffisdkstateblob_new(
    ): Short
    @JvmStatic
    external fun ffi_paykit_uniffi_contract_version(
    ): Int
}

// A JNA Library to expose the extern-C FFI definitions.
// This is an implementation detail which will be called internally by the public API.
internal object UniffiLib : Library {

    init {
        IntegrityCheckingUniffiLib
        Native.register(UniffiLib::class.java, findLibraryName("paykit"))
        // No need to check the contract version and checksums, since
        // we already did that with `IntegrityCheckingUniffiLib` above.
        uniffiCallbackInterfaceFfiSdkPaymentAdapter.register(this)
        uniffiCallbackInterfaceFfiSdkPubkySessionProvider.register(this)
        uniffiCallbackInterfaceFfiSdkStateBlobStore.register(this)
    }
    // The Cleaner for the whole library
    internal val CLEANER: UniffiCleaner by lazy {
        UniffiCleaner.create()
    }
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipaykitsdk(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipaykitsdk(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaykitsdk_new(
        `stateStore`: Pointer?,
        `sessionProvider`: Pointer?,
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter(
        `stateStore`: Pointer?,
        `sessionProvider`: Pointer?,
        `paymentAdapter`: Pointer?,
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter_and_pubky_client_config(
        `stateStore`: Pointer?,
        `sessionProvider`: Pointer?,
        `paymentAdapter`: Pointer?,
        `config`: RustBufferByValue,
        `pubkyClient`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaykitsdk_with_pubky_client_config(
        `stateStore`: Pointer?,
        `sessionProvider`: Pointer?,
        `config`: RustBufferByValue,
        `pubkyClient`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_accept_link_with_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_advance_link_handshake(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_block_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_config(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_contact_record(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_contact_records(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_current_private_payment_list(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_delete_paykit_blob(
        `ptr`: Pointer?,
        `uriOrPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_export_backup_state(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_paykit_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_file(
        `ptr`: Pointer?,
        `uri`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_follows(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_text(
        `ptr`: Pointer?,
        `uri`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_identity_status(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_initialize(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_initiate_link_with_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_linked_peers(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_pending_outbound_private_counterparties(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_process_outbound_private_messages(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_process_pending_private_messages(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_encrypted_link_recovery_marker(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_blob(
        `ptr`: Pointer?,
        `blobName`: RustBufferByValue,
        `bytes`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_profile(
        `ptr`: Pointer?,
        `profile`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_public_contact(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages_from_linked_peers(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_refresh_contact_paykit_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_remove_contact(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_remove_encrypted_link_recovery_marker(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_remove_public_contact(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_payment(
        `ptr`: Pointer?,
        `request`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `allowPubkyProfileFallback`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_state(
        `ptr`: Pointer?,
        `backup`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_save_contact(
        `ptr`: Pointer?,
        `update`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sign_out(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_public_contact_markers(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_public_endpoints(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_unblock_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipaymentpayload(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipaymentpayload(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaymentpayload_new(
        `text`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaymentpayload_export_text(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffiprivateoperationerror(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffiprivateoperationerror(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiprivateoperationerror_category(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiprivateoperationerror_code(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiprivateoperationerror_export_debug_details(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiprivateoperationerror_redacted_context(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipubkyauthrequest(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipubkyauthrequest(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkyauthrequest_authorization_url(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkyauthrequest_complete(
        `ptr`: Pointer?,
        `localSecretKey`: RustBufferByValue,
        `requiredCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipubkylocalsecretkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipubkylocalsecretkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipubkylocalsecretkey_new(
        `bytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkylocalsecretkey_export_bytes(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipubkysessionaccess(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipubkysessionaccess(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipubkysessionaccess_new(
        `sessionSecret`: RustBufferByValue,
        `localSecretKey`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionaccess_export_local_secret_key(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionaccess_export_session_secret(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffipubkysessionbootstrap(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipubkysessionbootstrap(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipubkysessionbootstrap_new(
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipubkysessionbootstrap_with_pubky_client_config(
        `pubkyClient`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_approve_auth(
        `ptr`: Pointer?,
        `authUrl`: RustBufferByValue,
        `expectedCapabilities`: RustBufferByValue,
        `localSecretKey`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_import_session(
        `ptr`: Pointer?,
        `sessionSecret`: RustBufferByValue,
        `localSecretKey`: RustBufferByValue,
        `requiredCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_resume_auth(
        `ptr`: Pointer?,
        `authorizationUrl`: RustBufferByValue,
        `expectedCapabilities`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_in(
        `ptr`: Pointer?,
        `localSecretKey`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_up(
        `ptr`: Pointer?,
        `localSecretKey`: Pointer?,
        `homeserverPublicKey`: RustBufferByValue,
        `signupCode`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_in_auth(
        `ptr`: Pointer?,
        `capabilities`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_up_auth(
        `ptr`: Pointer?,
        `capabilities`: RustBufferByValue,
        `homeserverPublicKey`: RustBufferByValue,
        `signupToken`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffireservationattribution(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffireservationattribution(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffireservationattribution_new(
        `fields`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffireservationattribution_export_fields(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffisdkbackupblob(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffisdkbackupblob(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffisdkbackupblob_new(
        `bytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkbackupblob_export_bytes(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffisdkpaymentadapter(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffisdkpaymentadapter(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_init_callback_vtable_ffisdkpaymentadapter(
        `vtable`: UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_current_receiving_details(
        `ptr`: Pointer?,
        `scope`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_reserve_receiving_details(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_cancel_receiving_detail_reservation(
        `ptr`: Pointer?,
        `cancellation`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_select_payment_endpoint_ids(
        `ptr`: Pointer?,
        `request`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_build_payment_target(
        `ptr`: Pointer?,
        `endpoint`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffisdkpubkysessionprovider(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffisdkpubkysessionprovider(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_init_callback_vtable_ffisdkpubkysessionprovider(
        `vtable`: UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpubkysessionprovider_load_session_access(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpubkysessionprovider_public_storage_available(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpubkysessionprovider_clear_session_access(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffisdkstateblob(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffisdkstateblob(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffisdkstateblob_new(
        `bytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkstateblob_export_bytes(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffisdkstateblobstore(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffisdkstateblobstore(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_init_callback_vtable_ffisdkstateblobstore(
        `vtable`: UniffiVTableCallbackInterfaceFfiSdkStateBlobStore,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkstateblobstore_load_state_blob(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkstateblobstore_save_state_blob_atomically(
        `ptr`: Pointer?,
        `blob`: Pointer?,
        `expectedRevision`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkstateblobstore_clear_state_blob(
        `ptr`: Pointer?,
        `expectedRevision`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_core_session_capabilities(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_default_config(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_default_pubky_client_config(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_derive_pubky_secret_key(
        `seed`: RustBufferByValue,
        `runtimeLabel`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_func_parse_pubky_auth_url(
        `authUrl`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_parse_pubky_resource(
        `uri`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_pubky_public_key_from_secret(
        `localSecretKey`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_required_session_capabilities(
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_resolve_pubky_url(
        `uri`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun ffi_paykit_rustbuffer_alloc(
        `size`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun ffi_paykit_rustbuffer_from_bytes(
        `bytes`: ForeignBytesByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun ffi_paykit_rustbuffer_free(
        `buf`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rustbuffer_reserve(
        `buf`: RustBufferByValue,
        `additional`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_u8(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_u8(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_u8(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_u8(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_i8(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_i8(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_i8(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_i8(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_u16(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_u16(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_u16(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_u16(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Short
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_i16(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_i16(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_i16(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_i16(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Short
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_u32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_u32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_u32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_u32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_i32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_i32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_i32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_i32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_u64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_u64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_u64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_u64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_i64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_i64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_i64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_i64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_f32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_f32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_f32(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_f32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Float
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_f64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_f64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_f64(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_f64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Double
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_pointer(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_pointer(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_pointer(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_pointer(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_rust_buffer(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_rust_buffer(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_rust_buffer(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_rust_buffer(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun ffi_paykit_rust_future_poll_void(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_cancel_void(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_free_void(
        `handle`: Long,
    ): Unit
    @JvmStatic
    external fun ffi_paykit_rust_future_complete_void(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
}

public fun uniffiEnsureInitialized() {
    UniffiLib
}

// Public interface members begin here.

internal const val IDX_CALLBACK_FREE = 0
// Callback return codes
internal const val UNIFFI_CALLBACK_SUCCESS = 0
internal const val UNIFFI_CALLBACK_ERROR = 1
internal const val UNIFFI_CALLBACK_UNEXPECTED_ERROR = 2

public abstract class FfiConverterCallbackInterface<CallbackInterface: Any>: FfiConverter<CallbackInterface, Long> {
    internal val handleMap = UniffiHandleMap<CallbackInterface>()

    internal fun drop(handle: Long) {
        handleMap.remove(handle)
    }

    override fun lift(value: Long): CallbackInterface {
        return handleMap.get(value)
    }

    override fun read(buf: ByteBuffer): CallbackInterface = lift(buf.getLong())

    override fun lower(value: CallbackInterface): Long = handleMap.insert(value)

    override fun allocationSize(value: CallbackInterface): ULong = 8UL

    override fun write(value: CallbackInterface, buf: ByteBuffer) {
        buf.putLong(lower(value))
    }
}
// The cleaner interface for Object finalization code to run.
// This is the entry point to any implementation that we're using.
//
// The cleaner registers disposables and returns cleanables, so now we are
// defining a `UniffiCleaner` with a `UniffiClenaer.Cleanable` to abstract the
// different implementations available at compile time.
public interface UniffiCleaner {
    public interface Cleanable {
        public fun clean()
    }

    public fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable

    public companion object
}
// The fallback Jna cleaner, which is available for both Android, and the JVM.
private class UniffiJnaCleaner : UniffiCleaner {
    private val cleaner = com.sun.jna.internal.Cleaner.getCleaner()

    override fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable =
        UniffiJnaCleanable(cleaner.register(resource, UniffiCleanerAction(disposable)))
}

private class UniffiJnaCleanable(
    private val cleanable: com.sun.jna.internal.Cleaner.Cleanable,
) : UniffiCleaner.Cleanable {
    override fun clean() = cleanable.clean()
}

private class UniffiCleanerAction(private val disposable: Disposable): Runnable {
    override fun run() {
        disposable.destroy()
    }
}

// The SystemCleaner, available from API Level 33.
// Some API Level 33 OSes do not support using it, so we require API Level 34.
@RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
private class AndroidSystemCleaner : UniffiCleaner {
    private val cleaner = android.system.SystemCleaner.cleaner()

    override fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable =
        AndroidSystemCleanable(cleaner.register(resource, UniffiCleanerAction(disposable)))
}

@RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
private class AndroidSystemCleanable(
    private val cleanable: java.lang.ref.Cleaner.Cleanable,
) : UniffiCleaner.Cleanable {
    override fun clean() = cleanable.clean()
}

private fun UniffiCleaner.Companion.create(): UniffiCleaner {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        try {
            return AndroidSystemCleaner()
        } catch (_: IllegalAccessError) {
            // (For Compose preview) Fallback to UniffiJnaCleaner if AndroidSystemCleaner is
            // unavailable, even for API level 34 or higher.
        }
    }
    return UniffiJnaCleaner()
}


public object FfiConverterUInt: FfiConverter<UInt, Int> {
    override fun lift(value: Int): UInt {
        return value.toUInt()
    }

    override fun read(buf: ByteBuffer): UInt {
        return lift(buf.getInt())
    }

    override fun lower(value: UInt): Int {
        return value.toInt()
    }

    override fun allocationSize(value: UInt): ULong = 4UL

    override fun write(value: UInt, buf: ByteBuffer) {
        buf.putInt(value.toInt())
    }
}


public object FfiConverterULong: FfiConverter<ULong, Long> {
    override fun lift(value: Long): ULong {
        return value.toULong()
    }

    override fun read(buf: ByteBuffer): ULong {
        return lift(buf.getLong())
    }

    override fun lower(value: ULong): Long {
        return value.toLong()
    }

    override fun allocationSize(value: ULong): ULong = 8UL

    override fun write(value: ULong, buf: ByteBuffer) {
        buf.putLong(value.toLong())
    }
}


public object FfiConverterBoolean: FfiConverter<Boolean, Byte> {
    override fun lift(value: Byte): Boolean {
        return value.toInt() != 0
    }

    override fun read(buf: ByteBuffer): Boolean {
        return lift(buf.get())
    }

    override fun lower(value: Boolean): Byte {
        return if (value) 1.toByte() else 0.toByte()
    }

    override fun allocationSize(value: Boolean): ULong = 1UL

    override fun write(value: Boolean, buf: ByteBuffer) {
        buf.put(lower(value))
    }
}


public object FfiConverterString: FfiConverter<String, RustBufferByValue> {
    // Note: we don't inherit from FfiConverterRustBuffer, because we use a
    // special encoding when lowering/lifting.  We can use `RustBuffer.len` to
    // store our length and avoid writing it out to the buffer.
    override fun lift(value: RustBufferByValue): String {
        try {
            require(value.len <= Int.MAX_VALUE) {
        val length = value.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
            val byteArr =  value.asByteBuffer()!!.get(value.len.toInt())
            return byteArr.decodeToString()
        } finally {
            RustBufferHelper.free(value)
        }
    }

    override fun read(buf: ByteBuffer): String {
        val len = buf.getInt()
        val byteArr = buf.get(len)
        return byteArr.decodeToString()
    }

    override fun lower(value: String): RustBufferByValue {
        // TODO: prevent allocating a new byte array here
        val encoded = value.encodeToByteArray(throwOnInvalidSequence = true)
        return RustBufferHelper.allocValue(encoded.size.toULong()).apply {
            asByteBuffer()!!.put(encoded)
        }
    }

    // We aren't sure exactly how many bytes our string will be once it's UTF-8
    // encoded.  Allocate 3 bytes per UTF-16 code unit which will always be
    // enough.
    override fun allocationSize(value: String): ULong {
        val sizeForLength = 4UL
        val sizeForString = value.length.toULong() * 3UL
        return sizeForLength + sizeForString
    }

    override fun write(value: String, buf: ByteBuffer) {
        // TODO: prevent allocating a new byte array here
        val encoded = value.encodeToByteArray(throwOnInvalidSequence = true)
        buf.putInt(encoded.size)
        buf.put(encoded)
    }
}


public object FfiConverterByteArray: FfiConverterRustBuffer<ByteArray> {
    override fun read(buf: ByteBuffer): ByteArray {
        val len = buf.getInt()
        val byteArr = buf.get(len)
        return byteArr
    }
    override fun allocationSize(value: ByteArray): ULong {
        return 4UL + value.size.toULong()
    }
    override fun write(value: ByteArray, buf: ByteBuffer) {
        buf.putInt(value.size)
        buf.put(value)
    }
}



/**
 * Stateful Paykit SDK runtime handle.
 */
public open class FfiPaykitSdk: Disposable, FfiPaykitSdkInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create an SDK runtime from platform storage/session callbacks.
     */
    public constructor(`stateStore`: FfiSdkStateBlobStore, `sessionProvider`: FfiSdkPubkySessionProvider, `config`: FfiPaykitSdkConfig) : this(
        uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_new(
                FfiConverterTypeFfiSdkStateBlobStore.lower(`stateStore`),
                FfiConverterTypeFfiSdkPubkySessionProvider.lower(`sessionProvider`),
                FfiConverterTypeFfiPaykitSdkConfig.lower(`config`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipaykitsdk(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipaykitsdk(pointer!!, status)
        }!!
    }


    /**
     * Start an Encrypted Link Handshake as the responder.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `acceptLinkWithPeer`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_accept_link_with_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Advance the stored Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `advanceLinkHandshake`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_advance_link_handshake(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Block a counterparty for local Paykit private workflows.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `blockPeer`(`counterparty`: kotlin.String): FfiLinkedPeerRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_block_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Return this runtime's configuration.
     */
    public override fun `config`(): FfiPaykitSdkConfig {
        return FfiConverterTypeFfiPaykitSdkConfig.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_config(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return one local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `contactRecord`(`publicKey`: kotlin.String): FfiContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_contact_record(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Return all local Contact Records.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `contactRecords`(): List<FfiContactRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_contact_records(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Return the latest valid Private Payment List view for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `currentPrivatePaymentList`(`counterparty`: kotlin.String): FfiPrivatePaymentListView? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_current_private_payment_list(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPrivatePaymentListView.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Delete a blob by `pubky://` URI or configured Paykit profile path.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `deletePaykitBlob`(`uriOrPath`: kotlin.String) {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_delete_paykit_blob(
                    thisPtr,
                    FfiConverterString.lower(`uriOrPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Return tracked Encrypted Link recovery marker state for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `encryptedLinkRecoveryMarkerStatus`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Queue the current complete Private Payment List for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `enqueuePrivatePaymentList`(`counterparty`: kotlin.String): FfiQueuedPrivateMessage {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiQueuedPrivateMessage.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Export SDK-managed backup state as an opaque blob.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `exportBackupState`(): FfiSdkBackupBlob {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_export_backup_state(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_pointer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_pointer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_pointer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_pointer(future) },
            // lift function
            { FfiConverterTypeFfiSdkBackupBlob.lift(it!!) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Paykit Profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPaykitProfile`(`publicKey`: kotlin.String): FfiPaykitProfileRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_paykit_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPaykitProfileRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Fetch public Pubky file bytes.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPubkyFile`(`uri`: kotlin.String): kotlin.ByteArray? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_file(
                    thisPtr,
                    FfiConverterString.lower(`uri`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalByteArray.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Fetch public Pubky app follows.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPubkyFollows`(`publicKey`: kotlin.String): List<kotlin.String> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_follows(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceString.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Pubky app profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPubkyProfile`(`publicKey`: kotlin.String): FfiPubkyProfileRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPubkyProfileRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Pubky UTF-8 text file.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPubkyText`(`uri`: kotlin.String): kotlin.String? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_pubky_text(
                    thisPtr,
                    FfiConverterString.lower(`uri`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalString.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Return current identity status, when initialized.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `identityStatus`(): FfiIdentityStatus? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_identity_status(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiIdentityStatus.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Initialize durable SDK identity state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `initialize`(): FfiInitializationReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_initialize(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiInitializationReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Start an Encrypted Link Handshake as the initiator.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `initiateLinkWithPeer`(`counterparty`: kotlin.String): FfiLinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_initiate_link_with_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * List locally tracked Linked Peer records.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `linkedPeers`(): List<FfiLinkedPeerRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_linked_peers(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeFfiLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Observe a counterparty's public recovery marker.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `observeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * List counterparties with queued private messages ready for retry.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `pendingOutboundPrivateCounterparties`(): List<kotlin.String> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_pending_outbound_private_counterparties(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceString.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Send queued outbound private messages for one counterparty in order.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `processOutboundPrivateMessages`(`counterparty`: kotlin.String): FfiOutboundPrivateSendReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_process_outbound_private_messages(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiOutboundPrivateSendReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Process queued outbound private messages for every pending counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `processPendingPrivateMessages`(): List<FfiOutboundPrivateCounterpartySendReport> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_process_pending_private_messages(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeFfiOutboundPrivateCounterpartySendReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Publish a minimal local recovery marker for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Publish a blob under this identity's Paykit profile namespace.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPaykitBlob`(`blobName`: kotlin.String, `bytes`: kotlin.ByteArray): FfiPaykitBlobRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_blob(
                    thisPtr,
                    FfiConverterString.lower(`blobName`),
                    FfiConverterByteArray.lower(`bytes`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPaykitBlobRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Publish this identity's Paykit Profile.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPaykitProfile`(`profile`: FfiPaykitProfile): FfiPaykitProfileRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_profile(
                    thisPtr,
                    FfiConverterTypeFfiPaykitProfile.lower(`profile`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPaykitProfileRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Publish a public Contact Marker for a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPublicContact`(`publicKey`: kotlin.String): FfiContactRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_public_contact(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Receive and durably persist available private messages.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receivePrivateMessages`(`counterparty`: kotlin.String): FfiPrivateStreamIntakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPrivateStreamIntakeReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Receive private messages from every locally linked counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receivePrivateMessagesFromLinkedPeers`(): List<FfiPrivateStreamCounterpartyIntakeReport> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages_from_linked_peers(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeFfiPrivateStreamCounterpartyIntakeReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Refresh the cached Paykit Profile for a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `refreshContactPaykitProfile`(`publicKey`: kotlin.String): FfiContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_refresh_contact_paykit_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Remove a local Contact Record when it has no public marker to clean up.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removeContact`(`publicKey`: kotlin.String): FfiContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_contact(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Remove the local public recovery marker for a counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String): FfiEncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Remove a public Contact Marker.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removePublicContact`(`publicKey`: kotlin.String): FfiContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_public_contact(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Resolve payable endpoints for one counterparty.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolveContactPayment`(`request`: FfiContactPaymentResolutionRequest): FfiContactPaymentResolution {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_payment(
                    thisPtr,
                    FfiConverterTypeFfiContactPaymentResolutionRequest.lower(`request`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiContactPaymentResolution.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Resolve display metadata for a contact.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolveContactProfile`(`publicKey`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): FfiContactProfileResolution? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterBoolean.lower(`allowPubkyProfileFallback`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactProfileResolution.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Restore SDK-managed backup state from an opaque blob.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `restoreBackupState`(`backup`: FfiSdkBackupBlob): FfiRestoreReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_state(
                    thisPtr,
                    FfiConverterTypeFfiSdkBackupBlob.lower(`backup`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiRestoreReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Save or update a local Contact Record.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `saveContact`(`update`: FfiContactUpdate): FfiContactRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_save_contact(
                    thisPtr,
                    FfiConverterTypeFfiContactUpdate.lower(`update`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Clear live Pubky session access and SDK-managed identity-scoped state.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signOut`(): FfiIdentityStatus {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sign_out(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiIdentityStatus.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Retry pending public Contact Marker publication/removal work.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPublicContactMarkers`(): List<FfiContactRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_public_contact_markers(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Publish current public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPublicEndpoints`(): FfiEndpointSyncReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_public_endpoints(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiEndpointSyncReport.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Remove a local peer block and return the peer to NotLinked.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `unblockPeer`(`counterparty`: kotlin.String): FfiLinkedPeerRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_unblock_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }






    public companion object {

        /**
         * Create an SDK runtime with payment adapter callbacks.
         */
        @Throws(PaykitFfiException::class)
        public fun `withPaymentAdapter`(`stateStore`: FfiSdkStateBlobStore, `sessionProvider`: FfiSdkPubkySessionProvider, `paymentAdapter`: FfiSdkPaymentAdapter, `config`: FfiPaykitSdkConfig): FfiPaykitSdk {
            return FfiConverterTypeFfiPaykitSdk.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter(
                    FfiConverterTypeFfiSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeFfiSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypeFfiSdkPaymentAdapter.lower(`paymentAdapter`),
                    FfiConverterTypeFfiPaykitSdkConfig.lower(`config`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


        /**
         * Create an SDK runtime with payment adapter callbacks and Pubky client configuration.
         */
        @Throws(PaykitFfiException::class)
        public fun `withPaymentAdapterAndPubkyClientConfig`(`stateStore`: FfiSdkStateBlobStore, `sessionProvider`: FfiSdkPubkySessionProvider, `paymentAdapter`: FfiSdkPaymentAdapter, `config`: FfiPaykitSdkConfig, `pubkyClient`: FfiPubkyClientConfig): FfiPaykitSdk {
            return FfiConverterTypeFfiPaykitSdk.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter_and_pubky_client_config(
                    FfiConverterTypeFfiSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeFfiSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypeFfiSdkPaymentAdapter.lower(`paymentAdapter`),
                    FfiConverterTypeFfiPaykitSdkConfig.lower(`config`),
                    FfiConverterTypeFfiPubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


        /**
         * Create an SDK runtime with explicit Pubky client configuration.
         */
        @Throws(PaykitFfiException::class)
        public fun `withPubkyClientConfig`(`stateStore`: FfiSdkStateBlobStore, `sessionProvider`: FfiSdkPubkySessionProvider, `config`: FfiPaykitSdkConfig, `pubkyClient`: FfiPubkyClientConfig): FfiPaykitSdk {
            return FfiConverterTypeFfiPaykitSdk.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_pubky_client_config(
                    FfiConverterTypeFfiSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeFfiSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypeFfiPaykitSdkConfig.lower(`config`),
                    FfiConverterTypeFfiPubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


    }

}





public object FfiConverterTypeFfiPaykitSdk: FfiConverter<FfiPaykitSdk, Pointer> {

    override fun lower(value: FfiPaykitSdk): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPaykitSdk {
        return FfiPaykitSdk(value)
    }

    override fun read(buf: ByteBuffer): FfiPaykitSdk {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPaykitSdk): ULong = 8UL

    override fun write(value: FfiPaykitSdk, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Payment adapter payload text with redacted debug output.
 */
public open class FfiPaymentPayload: Disposable, FfiPaymentPayloadInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create a payment payload from adapter-owned text.
     */
    public constructor(`text`: kotlin.String) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipaymentpayload_new(
                FfiConverterString.lower(`text`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipaymentpayload(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipaymentpayload(pointer!!, status)
        }!!
    }


    /**
     * Export the payload text for payment adapter execution.
     */
    public override fun `exportText`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipaymentpayload_export_text(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiPaymentPayload: FfiConverter<FfiPaymentPayload, Pointer> {

    override fun lower(value: FfiPaymentPayload): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPaymentPayload {
        return FfiPaymentPayload(value)
    }

    override fun read(buf: ByteBuffer): FfiPaymentPayload {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPaymentPayload): ULong = 8UL

    override fun write(value: FfiPaymentPayload, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Private workflow error with redacted default context.
 */
public open class FfiPrivateOperationError: Disposable, FfiPrivateOperationErrorInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffiprivateoperationerror(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffiprivateoperationerror(pointer!!, status)
        }!!
    }


    /**
     * Stable error category for app branching.
     */
    public override fun `category`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiprivateoperationerror_category(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Stable error code for app branching.
     */
    public override fun `code`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiprivateoperationerror_code(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Export raw debug details for explicit diagnostic handling.
     */
    public override fun `exportDebugDetails`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiprivateoperationerror_export_debug_details(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Redacted error context safe for normal UI/log surfaces.
     */
    public override fun `redactedContext`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiprivateoperationerror_redacted_context(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiPrivateOperationError: FfiConverter<FfiPrivateOperationError, Pointer> {

    override fun lower(value: FfiPrivateOperationError): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPrivateOperationError {
        return FfiPrivateOperationError(value)
    }

    override fun read(buf: ByteBuffer): FfiPrivateOperationError {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPrivateOperationError): ULong = 8UL

    override fun write(value: FfiPrivateOperationError, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Pending Pubky auth request.
 */
public open class FfiPubkyAuthRequest: Disposable, FfiPubkyAuthRequestInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipubkyauthrequest(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipubkyauthrequest(pointer!!, status)
        }!!
    }


    /**
     * Return the auth URL to show as a deeplink or QR code.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `authorizationUrl`(): kotlin.String {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkyauthrequest_authorization_url(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterString.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Wait for auth approval and validate the resulting session capabilities.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `complete`(`localSecretKey`: FfiPubkyLocalSecretKey?, `requiredCapabilities`: kotlin.String): FfiPubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkyauthrequest_complete(
                    thisPtr,
                    FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }







    public companion object

}





public object FfiConverterTypeFfiPubkyAuthRequest: FfiConverter<FfiPubkyAuthRequest, Pointer> {

    override fun lower(value: FfiPubkyAuthRequest): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPubkyAuthRequest {
        return FfiPubkyAuthRequest(value)
    }

    override fun read(buf: ByteBuffer): FfiPubkyAuthRequest {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPubkyAuthRequest): ULong = 8UL

    override fun write(value: FfiPubkyAuthRequest, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Local Pubky secret key bytes supplied by platform secure storage.
 */
public open class FfiPubkyLocalSecretKey: Disposable, FfiPubkyLocalSecretKeyInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create a local Pubky secret key from platform secure storage bytes.
     */
    public constructor(`bytes`: kotlin.ByteArray) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipubkylocalsecretkey_new(
                FfiConverterByteArray.lower(`bytes`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipubkylocalsecretkey(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipubkylocalsecretkey(pointer!!, status)
        }!!
    }


    /**
     * Export the raw bytes for platform secure storage.
     */
    public override fun `exportBytes`(): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkylocalsecretkey_export_bytes(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiPubkyLocalSecretKey: FfiConverter<FfiPubkyLocalSecretKey, Pointer> {

    override fun lower(value: FfiPubkyLocalSecretKey): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPubkyLocalSecretKey {
        return FfiPubkyLocalSecretKey(value)
    }

    override fun read(buf: ByteBuffer): FfiPubkyLocalSecretKey {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPubkyLocalSecretKey): ULong = 8UL

    override fun write(value: FfiPubkyLocalSecretKey, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Live Pubky access material supplied by platform session storage.
 */
public open class FfiPubkySessionAccess: Disposable, FfiPubkySessionAccessInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create session access material from platform secure storage.
     */
    public constructor(`sessionSecret`: kotlin.String, `localSecretKey`: FfiPubkyLocalSecretKey?) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipubkysessionaccess_new(
                FfiConverterString.lower(`sessionSecret`),
                FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipubkysessionaccess(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipubkysessionaccess(pointer!!, status)
        }!!
    }


    /**
     * Export the local Pubky secret key, when available.
     */
    public override fun `exportLocalSecretKey`(): FfiPubkyLocalSecretKey? {
        return FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionaccess_export_local_secret_key(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Export the Pubky session bearer secret for platform secure storage.
     */
    public override fun `exportSessionSecret`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionaccess_export_session_secret(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiPubkySessionAccess: FfiConverter<FfiPubkySessionAccess, Pointer> {

    override fun lower(value: FfiPubkySessionAccess): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPubkySessionAccess {
        return FfiPubkySessionAccess(value)
    }

    override fun read(buf: ByteBuffer): FfiPubkySessionAccess {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPubkySessionAccess): ULong = 8UL

    override fun write(value: FfiPubkySessionAccess, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Pubky session bootstrap helper.
 */
public open class FfiPubkySessionBootstrap: Disposable, FfiPubkySessionBootstrapInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create a Pubky session bootstrap helper.
     */
    public constructor() : this(
        uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipubkysessionbootstrap_new(
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffipubkysessionbootstrap(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipubkysessionbootstrap(pointer!!, status)
        }!!
    }


    /**
     * Approve a Pubky auth URL with this local secret key.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `approveAuth`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: FfiPubkyLocalSecretKey) {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_approve_auth(
                    thisPtr,
                    FfiConverterString.lower(`authUrl`),
                    FfiConverterString.lower(`expectedCapabilities`),
                    FfiConverterTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Import an exported Pubky session secret.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `importSession`(`sessionSecret`: kotlin.String, `localSecretKey`: FfiPubkyLocalSecretKey?, `requiredCapabilities`: kotlin.String): FfiPubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_import_session(
                    thisPtr,
                    FfiConverterString.lower(`sessionSecret`),
                    FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Resume a short-lived auth flow from its authorization URL.
     */
    @Throws(PaykitFfiException::class)
    public override fun `resumeAuth`(`authorizationUrl`: kotlin.String, `expectedCapabilities`: kotlin.String): FfiPubkyAuthRequest {
        return FfiConverterTypeFfiPubkyAuthRequest.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_resume_auth(
                    it,
                    FfiConverterString.lower(`authorizationUrl`),
                    FfiConverterString.lower(`expectedCapabilities`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    /**
     * Sign in with a local Pubky secret key and return session access material.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signIn`(`localSecretKey`: FfiPubkyLocalSecretKey): FfiPubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_in(
                    thisPtr,
                    FfiConverterTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Sign up on a homeserver and return session access material.
     */
    @Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signUp`(`localSecretKey`: FfiPubkyLocalSecretKey, `homeserverPublicKey`: kotlin.String, `signupCode`: kotlin.String?): FfiPubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_up(
                    thisPtr,
                    FfiConverterTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterString.lower(`homeserverPublicKey`),
                    FfiConverterOptionalString.lower(`signupCode`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeFfiPubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitFfiExceptionErrorHandler,
        )
    }

    /**
     * Start a sign-in auth flow for an external signer.
     */
    @Throws(PaykitFfiException::class)
    public override fun `startSignInAuth`(`capabilities`: kotlin.String): FfiPubkyAuthRequest {
        return FfiConverterTypeFfiPubkyAuthRequest.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_in_auth(
                    it,
                    FfiConverterString.lower(`capabilities`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    /**
     * Start a signup auth flow for an external signer.
     */
    @Throws(PaykitFfiException::class)
    public override fun `startSignUpAuth`(`capabilities`: kotlin.String, `homeserverPublicKey`: kotlin.String, `signupToken`: kotlin.String?): FfiPubkyAuthRequest {
        return FfiConverterTypeFfiPubkyAuthRequest.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_up_auth(
                    it,
                    FfiConverterString.lower(`capabilities`),
                    FfiConverterString.lower(`homeserverPublicKey`),
                    FfiConverterOptionalString.lower(`signupToken`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }






    public companion object {

        /**
         * Create a Pubky session bootstrap helper with explicit Pubky client configuration.
         */
        @Throws(PaykitFfiException::class)
        public fun `withPubkyClientConfig`(`pubkyClient`: FfiPubkyClientConfig): FfiPubkySessionBootstrap {
            return FfiConverterTypeFfiPubkySessionBootstrap.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipubkysessionbootstrap_with_pubky_client_config(
                    FfiConverterTypeFfiPubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


    }

}





public object FfiConverterTypeFfiPubkySessionBootstrap: FfiConverter<FfiPubkySessionBootstrap, Pointer> {

    override fun lower(value: FfiPubkySessionBootstrap): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiPubkySessionBootstrap {
        return FfiPubkySessionBootstrap(value)
    }

    override fun read(buf: ByteBuffer): FfiPubkySessionBootstrap {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiPubkySessionBootstrap): ULong = 8UL

    override fun write(value: FfiPubkySessionBootstrap, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Reservation attribution metadata with redacted debug output.
 */
public open class FfiReservationAttribution: Disposable, FfiReservationAttributionInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create reservation attribution metadata.
     */
    public constructor(`fields`: Map<kotlin.String, kotlin.String>) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffireservationattribution_new(
                FfiConverterMapStringString.lower(`fields`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffireservationattribution(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffireservationattribution(pointer!!, status)
        }!!
    }


    /**
     * Export attribution fields for payment adapter cleanup.
     */
    public override fun `exportFields`(): Map<kotlin.String, kotlin.String> {
        return FfiConverterMapStringString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffireservationattribution_export_fields(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiReservationAttribution: FfiConverter<FfiReservationAttribution, Pointer> {

    override fun lower(value: FfiReservationAttribution): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiReservationAttribution {
        return FfiReservationAttribution(value)
    }

    override fun read(buf: ByteBuffer): FfiReservationAttribution {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiReservationAttribution): ULong = 8UL

    override fun write(value: FfiReservationAttribution, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * SDK backup blob owned by the app.
 */
public open class FfiSdkBackupBlob: Disposable, FfiSdkBackupBlobInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create an SDK backup blob from app-owned bytes.
     */
    public constructor(`bytes`: kotlin.ByteArray) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffisdkbackupblob_new(
                FfiConverterByteArray.lower(`bytes`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffisdkbackupblob(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffisdkbackupblob(pointer!!, status)
        }!!
    }


    /**
     * Export the raw bytes for app-controlled backup storage.
     */
    public override fun `exportBytes`(): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkbackupblob_export_bytes(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiSdkBackupBlob: FfiConverter<FfiSdkBackupBlob, Pointer> {

    override fun lower(value: FfiSdkBackupBlob): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiSdkBackupBlob {
        return FfiSdkBackupBlob(value)
    }

    override fun read(buf: ByteBuffer): FfiSdkBackupBlob {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiSdkBackupBlob): ULong = 8UL

    override fun write(value: FfiSdkBackupBlob, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Platform-owned payment adapter callbacks.
 */
public open class FfiSdkPaymentAdapterImpl: Disposable, FfiSdkPaymentAdapter {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffisdkpaymentadapter(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffisdkpaymentadapter(pointer!!, status)
        }!!
    }


    /**
     * Return current receiving details for a scope.
     */
    @Throws(PaykitFfiException::class)
    public override fun `currentReceivingDetails`(`scope`: FfiReceivingDetailScope): List<FfiReceivingDetail> {
        return FfiConverterSequenceTypeFfiReceivingDetail.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_current_receiving_details(
                    it,
                    FfiConverterTypeFfiReceivingDetailScope.lower(`scope`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Reserve receiving details for a counterparty's Private Payment List.
     */
    @Throws(PaykitFfiException::class)
    public override fun `reserveReceivingDetails`(`counterparty`: kotlin.String): List<FfiPaymentEndpointReservation>? {
        return FfiConverterOptionalSequenceTypeFfiPaymentEndpointReservation.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_reserve_receiving_details(
                    it,
                    FfiConverterString.lower(`counterparty`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Cancel a previously reserved receiving detail.
     */
    @Throws(PaykitFfiException::class)
    public override fun `cancelReceivingDetailReservation`(`cancellation`: FfiPaymentEndpointReservationCancellation) {
        callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_cancel_receiving_detail_reservation(
                    it,
                    FfiConverterTypeFfiPaymentEndpointReservationCancellation.lower(`cancellation`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    /**
     * Return payable candidate ids in adapter-preferred order.
     */
    @Throws(PaykitFfiException::class)
    public override fun `selectPaymentEndpointIds`(`request`: FfiPaymentEndpointSelectionRequest): List<kotlin.String> {
        return FfiConverterSequenceString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_select_payment_endpoint_ids(
                    it,
                    FfiConverterTypeFfiPaymentEndpointSelectionRequest.lower(`request`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Build a payment target from a payable endpoint.
     */
    @Throws(PaykitFfiException::class)
    public override fun `buildPaymentTarget`(`endpoint`: FfiPaymentEndpointCandidate): FfiPaymentTarget {
        return FfiConverterTypeFfiPaymentTarget.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_build_payment_target(
                    it,
                    FfiConverterTypeFfiPaymentEndpointCandidate.lower(`endpoint`),
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiSdkPaymentAdapter: FfiConverter<FfiSdkPaymentAdapter, Pointer> {
    internal val handleMap = UniffiHandleMap<FfiSdkPaymentAdapter>()

    override fun lower(value: FfiSdkPaymentAdapter): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): FfiSdkPaymentAdapter {
        return FfiSdkPaymentAdapterImpl(value)
    }

    override fun read(buf: ByteBuffer): FfiSdkPaymentAdapter {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiSdkPaymentAdapter): ULong = 8UL

    override fun write(value: FfiSdkPaymentAdapter, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}


// Put the implementation in an object so we don't pollute the top-level namespace
internal object uniffiCallbackInterfaceFfiSdkPaymentAdapter {
    internal object `currentReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0 {
        override fun callback (
            `uniffiHandle`: Long,
            `scope`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`currentReceivingDetails`(
                    FfiConverterTypeFfiReceivingDetailScope.lift(`scope`),
                )
            }
            val writeReturn = { uniffiResultValue: List<FfiReceivingDetail> ->
                uniffiOutReturn.setValue(FfiConverterSequenceTypeFfiReceivingDetail.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `reserveReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1 {
        override fun callback (
            `uniffiHandle`: Long,
            `counterparty`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`reserveReceivingDetails`(
                    FfiConverterString.lift(`counterparty`),
                )
            }
            val writeReturn = { uniffiResultValue: List<FfiPaymentEndpointReservation>? ->
                uniffiOutReturn.setValue(FfiConverterOptionalSequenceTypeFfiPaymentEndpointReservation.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `cancelReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2 {
        override fun callback (
            `uniffiHandle`: Long,
            `cancellation`: RustBufferByValue,
            `uniffiOutReturn`: Pointer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`cancelReceivingDetailReservation`(
                    FfiConverterTypeFfiPaymentEndpointReservationCancellation.lift(`cancellation`),
                )
            }
            val writeReturn = { _: Unit ->
                @Suppress("UNUSED_EXPRESSION")
                uniffiOutReturn
                Unit
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `selectPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3 {
        override fun callback (
            `uniffiHandle`: Long,
            `request`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`selectPaymentEndpointIds`(
                    FfiConverterTypeFfiPaymentEndpointSelectionRequest.lift(`request`),
                )
            }
            val writeReturn = { uniffiResultValue: List<kotlin.String> ->
                uniffiOutReturn.setValue(FfiConverterSequenceString.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `buildPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4 {
        override fun callback (
            `uniffiHandle`: Long,
            `endpoint`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`buildPaymentTarget`(
                    FfiConverterTypeFfiPaymentEndpointCandidate.lift(`endpoint`),
                )
            }
            val writeReturn = { uniffiResultValue: FfiPaymentTarget ->
                uniffiOutReturn.setValue(FfiConverterTypeFfiPaymentTarget.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeFfiSdkPaymentAdapter.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter(
        `currentReceivingDetails`,
        `reserveReceivingDetails`,
        `cancelReceivingDetailReservation`,
        `selectPaymentEndpointIds`,
        `buildPaymentTarget`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_paykit_fn_init_callback_vtable_ffisdkpaymentadapter(vtable)
    }
}



/**
 * Platform-owned Pubky session provider.
 */
public open class FfiSdkPubkySessionProviderImpl: Disposable, FfiSdkPubkySessionProvider {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffisdkpubkysessionprovider(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffisdkpubkysessionprovider(pointer!!, status)
        }!!
    }


    /**
     * Load current live Pubky session access, when available.
     */
    @Throws(PaykitFfiException::class)
    public override fun `loadSessionAccess`(): FfiPubkySessionAccess? {
        return FfiConverterOptionalTypeFfiPubkySessionAccess.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpubkysessionprovider_load_session_access(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Report whether unauthenticated public Pubky storage can be used.
     */
    @Throws(PaykitFfiException::class)
    public override fun `publicStorageAvailable`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpubkysessionprovider_public_storage_available(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Clear platform session access during explicit SDK sign-out.
     */
    @Throws(PaykitFfiException::class)
    public override fun `clearSessionAccess`() {
        callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpubkysessionprovider_clear_session_access(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }







    public companion object

}





public object FfiConverterTypeFfiSdkPubkySessionProvider: FfiConverter<FfiSdkPubkySessionProvider, Pointer> {
    internal val handleMap = UniffiHandleMap<FfiSdkPubkySessionProvider>()

    override fun lower(value: FfiSdkPubkySessionProvider): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): FfiSdkPubkySessionProvider {
        return FfiSdkPubkySessionProviderImpl(value)
    }

    override fun read(buf: ByteBuffer): FfiSdkPubkySessionProvider {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiSdkPubkySessionProvider): ULong = 8UL

    override fun write(value: FfiSdkPubkySessionProvider, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}


// Put the implementation in an object so we don't pollute the top-level namespace
internal object uniffiCallbackInterfaceFfiSdkPubkySessionProvider {
    internal object `loadSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod0 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPubkySessionProvider.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`loadSessionAccess`(
                )
            }
            val writeReturn = { uniffiResultValue: FfiPubkySessionAccess? ->
                uniffiOutReturn.setValue(FfiConverterOptionalTypeFfiPubkySessionAccess.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `publicStorageAvailable`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod1 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: ByteByReference,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPubkySessionProvider.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`publicStorageAvailable`(
                )
            }
            val writeReturn = { uniffiResultValue: kotlin.Boolean ->
                uniffiOutReturn.setValue(FfiConverterBoolean.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `clearSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod2 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: Pointer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkPubkySessionProvider.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`clearSessionAccess`(
                )
            }
            val writeReturn = { _: Unit ->
                @Suppress("UNUSED_EXPRESSION")
                uniffiOutReturn
                Unit
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeFfiSdkPubkySessionProvider.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceFfiSdkPubkySessionProvider(
        `loadSessionAccess`,
        `publicStorageAvailable`,
        `clearSessionAccess`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_paykit_fn_init_callback_vtable_ffisdkpubkysessionprovider(vtable)
    }
}



/**
 * SDK state blob owned by platform storage.
 */
public open class FfiSdkStateBlob: Disposable, FfiSdkStateBlobInterface {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }
    /**
     * Create an SDK state blob from platform storage bytes.
     */
    public constructor(`bytes`: kotlin.ByteArray) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffisdkstateblob_new(
                FfiConverterByteArray.lower(`bytes`),
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffisdkstateblob(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffisdkstateblob(pointer!!, status)
        }!!
    }


    /**
     * Export the raw bytes for platform storage.
     */
    public override fun `exportBytes`(): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkstateblob_export_bytes(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiSdkStateBlob: FfiConverter<FfiSdkStateBlob, Pointer> {

    override fun lower(value: FfiSdkStateBlob): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FfiSdkStateBlob {
        return FfiSdkStateBlob(value)
    }

    override fun read(buf: ByteBuffer): FfiSdkStateBlob {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiSdkStateBlob): ULong = 8UL

    override fun write(value: FfiSdkStateBlob, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Platform-owned durable blob store for SDK state.
 */
public open class FfiSdkStateBlobStoreImpl: Disposable, FfiSdkStateBlobStore {

    public constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    public constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.uniffi_paykit_fn_free_ffisdkstateblobstore(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffisdkstateblobstore(pointer!!, status)
        }!!
    }


    /**
     * Load the current SDK state blob, when one exists.
     */
    @Throws(PaykitFfiException::class)
    public override fun `loadStateBlob`(): FfiSdkStateBlobSnapshot? {
        return FfiConverterOptionalTypeFfiSdkStateBlobSnapshot.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkstateblobstore_load_state_blob(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Atomically save a new SDK state blob.
     *
     * `expected_revision` is `None` when no previous blob was loaded. The
     * platform store should reject the write if the stored revision changed.
     */
    @Throws(PaykitFfiException::class)
    public override fun `saveStateBlobAtomically`(`blob`: FfiSdkStateBlob, `expectedRevision`: kotlin.String?): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkstateblobstore_save_state_blob_atomically(
                    it,
                    FfiConverterTypeFfiSdkStateBlob.lower(`blob`),
                    FfiConverterOptionalString.lower(`expectedRevision`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Atomically clear the SDK state blob.
     *
     * The platform store should reject the clear if the stored revision does
     * not match `expected_revision`.
     */
    @Throws(PaykitFfiException::class)
    public override fun `clearStateBlob`(`expectedRevision`: kotlin.String?): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkstateblobstore_clear_state_blob(
                    it,
                    FfiConverterOptionalString.lower(`expectedRevision`),
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeFfiSdkStateBlobStore: FfiConverter<FfiSdkStateBlobStore, Pointer> {
    internal val handleMap = UniffiHandleMap<FfiSdkStateBlobStore>()

    override fun lower(value: FfiSdkStateBlobStore): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): FfiSdkStateBlobStore {
        return FfiSdkStateBlobStoreImpl(value)
    }

    override fun read(buf: ByteBuffer): FfiSdkStateBlobStore {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FfiSdkStateBlobStore): ULong = 8UL

    override fun write(value: FfiSdkStateBlobStore, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}


// Put the implementation in an object so we don't pollute the top-level namespace
internal object uniffiCallbackInterfaceFfiSdkStateBlobStore {
    internal object `loadStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkStateBlobStore.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`loadStateBlob`(
                )
            }
            val writeReturn = { uniffiResultValue: FfiSdkStateBlobSnapshot? ->
                uniffiOutReturn.setValue(FfiConverterOptionalTypeFfiSdkStateBlobSnapshot.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `saveStateBlobAtomically`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1 {
        override fun callback (
            `uniffiHandle`: Long,
            `blob`: Pointer?,
            `expectedRevision`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkStateBlobStore.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`saveStateBlobAtomically`(
                    FfiConverterTypeFfiSdkStateBlob.lift(`blob`!!),
                    FfiConverterOptionalString.lift(`expectedRevision`),
                )
            }
            val writeReturn = { uniffiResultValue: kotlin.String ->
                uniffiOutReturn.setValue(FfiConverterString.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object `clearStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod2 {
        override fun callback (
            `uniffiHandle`: Long,
            `expectedRevision`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeFfiSdkStateBlobStore.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`clearStateBlob`(
                    FfiConverterOptionalString.lift(`expectedRevision`),
                )
            }
            val writeReturn = { uniffiResultValue: kotlin.String ->
                uniffiOutReturn.setValue(FfiConverterString.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitFfiException -> FfiConverterTypePaykitFfiError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeFfiSdkStateBlobStore.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceFfiSdkStateBlobStore(
        `loadStateBlob`,
        `saveStateBlobAtomically`,
        `clearStateBlob`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_paykit_fn_init_callback_vtable_ffisdkstateblobstore(vtable)
    }
}




public object FfiConverterTypeFfiContactPaymentResolution: FfiConverterRustBuffer<FfiContactPaymentResolution> {
    override fun read(buf: ByteBuffer): FfiContactPaymentResolution {
        return FfiContactPaymentResolution(
            FfiConverterTypeFfiContactPaymentResolutionStatus.read(buf),
            FfiConverterTypeFfiContactPaymentResolutionPrivateState.read(buf),
            FfiConverterSequenceTypeFfiResolvedPaymentEndpoint.read(buf),
        )
    }

    override fun allocationSize(value: FfiContactPaymentResolution): ULong = (
            FfiConverterTypeFfiContactPaymentResolutionStatus.allocationSize(value.`status`) +
            FfiConverterTypeFfiContactPaymentResolutionPrivateState.allocationSize(value.`privateState`) +
            FfiConverterSequenceTypeFfiResolvedPaymentEndpoint.allocationSize(value.`payableEndpoints`)
    )

    override fun write(value: FfiContactPaymentResolution, buf: ByteBuffer) {
        FfiConverterTypeFfiContactPaymentResolutionStatus.write(value.`status`, buf)
        FfiConverterTypeFfiContactPaymentResolutionPrivateState.write(value.`privateState`, buf)
        FfiConverterSequenceTypeFfiResolvedPaymentEndpoint.write(value.`payableEndpoints`, buf)
    }
}




public object FfiConverterTypeFfiContactPaymentResolutionRequest: FfiConverterRustBuffer<FfiContactPaymentResolutionRequest> {
    override fun read(buf: ByteBuffer): FfiContactPaymentResolutionRequest {
        return FfiContactPaymentResolutionRequest(
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPaymentAmountContext.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: FfiContactPaymentResolutionRequest): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalTypeFfiPaymentAmountContext.allocationSize(value.`amount`) +
            FfiConverterBoolean.allocationSize(value.`includePublicEndpoints`)
    )

    override fun write(value: FfiContactPaymentResolutionRequest, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterOptionalTypeFfiPaymentAmountContext.write(value.`amount`, buf)
        FfiConverterBoolean.write(value.`includePublicEndpoints`, buf)
    }
}




public object FfiConverterTypeFfiContactProfileResolution: FfiConverterRustBuffer<FfiContactProfileResolution> {
    override fun read(buf: ByteBuffer): FfiContactProfileResolution {
        return FfiContactProfileResolution(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiContactProfileSource.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaykitProfile.read(buf),
            FfiConverterOptionalTypeFfiPubkyProfile.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiContactProfileResolution): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypeFfiContactProfileSource.allocationSize(value.`source`) +
            FfiConverterOptionalString.allocationSize(value.`displayName`) +
            FfiConverterOptionalString.allocationSize(value.`imageUri`) +
            FfiConverterOptionalTypeFfiPaykitProfile.allocationSize(value.`paykitProfile`) +
            FfiConverterOptionalTypeFfiPubkyProfile.allocationSize(value.`pubkyProfile`) +
            FfiConverterString.allocationSize(value.`fetchedAt`)
    )

    override fun write(value: FfiContactProfileResolution, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypeFfiContactProfileSource.write(value.`source`, buf)
        FfiConverterOptionalString.write(value.`displayName`, buf)
        FfiConverterOptionalString.write(value.`imageUri`, buf)
        FfiConverterOptionalTypeFfiPaykitProfile.write(value.`paykitProfile`, buf)
        FfiConverterOptionalTypeFfiPubkyProfile.write(value.`pubkyProfile`, buf)
        FfiConverterString.write(value.`fetchedAt`, buf)
    }
}




public object FfiConverterTypeFfiContactRecord: FfiConverterRustBuffer<FfiContactRecord> {
    override fun read(buf: ByteBuffer): FfiContactRecord {
        return FfiContactRecord(
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaykitProfile.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPublicationStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiContactRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterOptionalString.allocationSize(value.`label`) +
            FfiConverterOptionalTypeFfiPaykitProfile.allocationSize(value.`profile`) +
            FfiConverterOptionalString.allocationSize(value.`profileFetchedAt`) +
            FfiConverterString.allocationSize(value.`createdAt`) +
            FfiConverterString.allocationSize(value.`updatedAt`) +
            FfiConverterTypeFfiPublicationStatus.allocationSize(value.`publicContactMarkerStatus`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactPublishedAt`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactRemovedAt`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactLastError`)
    )

    override fun write(value: FfiContactRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterOptionalString.write(value.`label`, buf)
        FfiConverterOptionalTypeFfiPaykitProfile.write(value.`profile`, buf)
        FfiConverterOptionalString.write(value.`profileFetchedAt`, buf)
        FfiConverterString.write(value.`createdAt`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
        FfiConverterTypeFfiPublicationStatus.write(value.`publicContactMarkerStatus`, buf)
        FfiConverterOptionalString.write(value.`publicContactPublishedAt`, buf)
        FfiConverterOptionalString.write(value.`publicContactRemovedAt`, buf)
        FfiConverterOptionalString.write(value.`publicContactLastError`, buf)
    }
}




public object FfiConverterTypeFfiContactUpdate: FfiConverterRustBuffer<FfiContactUpdate> {
    override fun read(buf: ByteBuffer): FfiContactUpdate {
        return FfiContactUpdate(
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiContactUpdate): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterOptionalString.allocationSize(value.`label`)
    )

    override fun write(value: FfiContactUpdate, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterOptionalString.write(value.`label`, buf)
    }
}




public object FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport: FfiConverterRustBuffer<FfiEncryptedLinkRecoveryMarkerReport> {
    override fun read(buf: ByteBuffer): FfiEncryptedLinkRecoveryMarkerReport {
        return FfiEncryptedLinkRecoveryMarkerReport(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiLinkedPeerState.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: FfiEncryptedLinkRecoveryMarkerReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterTypeFfiLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterOptionalString.allocationSize(value.`localAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`localMarkerCreatedAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`localMarkerLastError`) +
            FfiConverterOptionalString.allocationSize(value.`remoteAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`remoteMarkerObservedAt`) +
            FfiConverterBoolean.allocationSize(value.`remoteMarkerChanged`)
    )

    override fun write(value: FfiEncryptedLinkRecoveryMarkerReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterTypeFfiLinkedPeerState.write(value.`state`, buf)
        FfiConverterOptionalString.write(value.`localAttemptId`, buf)
        FfiConverterOptionalString.write(value.`localMarkerCreatedAt`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`localMarkerLastError`, buf)
        FfiConverterOptionalString.write(value.`remoteAttemptId`, buf)
        FfiConverterOptionalString.write(value.`remoteMarkerObservedAt`, buf)
        FfiConverterBoolean.write(value.`remoteMarkerChanged`, buf)
    }
}




public object FfiConverterTypeFfiEndpointSyncChange: FfiConverterRustBuffer<FfiEndpointSyncChange> {
    override fun read(buf: ByteBuffer): FfiEndpointSyncChange {
        return FfiEndpointSyncChange(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPublicationStatus.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiEndpointSyncChange): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypeFfiPublicationStatus.allocationSize(value.`status`) +
            FfiConverterOptionalString.allocationSize(value.`error`)
    )

    override fun write(value: FfiEndpointSyncChange, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypeFfiPublicationStatus.write(value.`status`, buf)
        FfiConverterOptionalString.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiEndpointSyncReport: FfiConverterRustBuffer<FfiEndpointSyncReport> {
    override fun read(buf: ByteBuffer): FfiEndpointSyncReport {
        return FfiEndpointSyncReport(
            FfiConverterSequenceTypeFfiEndpointSyncChange.read(buf),
            FfiConverterSequenceTypeFfiEndpointSyncChange.read(buf),
            FfiConverterSequenceTypeFfiEndpointSyncChange.read(buf),
        )
    }

    override fun allocationSize(value: FfiEndpointSyncReport): ULong = (
            FfiConverterSequenceTypeFfiEndpointSyncChange.allocationSize(value.`published`) +
            FfiConverterSequenceTypeFfiEndpointSyncChange.allocationSize(value.`removed`) +
            FfiConverterSequenceTypeFfiEndpointSyncChange.allocationSize(value.`failed`)
    )

    override fun write(value: FfiEndpointSyncReport, buf: ByteBuffer) {
        FfiConverterSequenceTypeFfiEndpointSyncChange.write(value.`published`, buf)
        FfiConverterSequenceTypeFfiEndpointSyncChange.write(value.`removed`, buf)
        FfiConverterSequenceTypeFfiEndpointSyncChange.write(value.`failed`, buf)
    }
}




public object FfiConverterTypeFfiEventIdConflict: FfiConverterRustBuffer<FfiEventIdConflict> {
    override fun read(buf: ByteBuffer): FfiEventIdConflict {
        return FfiEventIdConflict(
            FfiConverterString.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: FfiEventIdConflict): ULong = (
            FfiConverterString.allocationSize(value.`eventId`) +
            FfiConverterULong.allocationSize(value.`firstStreamItemId`) +
            FfiConverterULong.allocationSize(value.`conflictingStreamItemId`)
    )

    override fun write(value: FfiEventIdConflict, buf: ByteBuffer) {
        FfiConverterString.write(value.`eventId`, buf)
        FfiConverterULong.write(value.`firstStreamItemId`, buf)
        FfiConverterULong.write(value.`conflictingStreamItemId`, buf)
    }
}




public object FfiConverterTypeFfiIdentityStatus: FfiConverterRustBuffer<FfiIdentityStatus> {
    override fun read(buf: ByteBuffer): FfiIdentityStatus {
        return FfiIdentityStatus(
            FfiConverterOptionalString.read(buf),
            FfiConverterTypeFfiPubkyIdentityCapability.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: FfiIdentityStatus): ULong = (
            FfiConverterOptionalString.allocationSize(value.`publicKey`) +
            FfiConverterTypeFfiPubkyIdentityCapability.allocationSize(value.`capability`) +
            FfiConverterBoolean.allocationSize(value.`liveSessionAvailable`) +
            FfiConverterBoolean.allocationSize(value.`privateLinkCapable`)
    )

    override fun write(value: FfiIdentityStatus, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`publicKey`, buf)
        FfiConverterTypeFfiPubkyIdentityCapability.write(value.`capability`, buf)
        FfiConverterBoolean.write(value.`liveSessionAvailable`, buf)
        FfiConverterBoolean.write(value.`privateLinkCapable`, buf)
    }
}




public object FfiConverterTypeFfiInitializationReport: FfiConverterRustBuffer<FfiInitializationReport> {
    override fun read(buf: ByteBuffer): FfiInitializationReport {
        return FfiInitializationReport(
            FfiConverterTypeFfiIdentityStatus.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: FfiInitializationReport): ULong = (
            FfiConverterTypeFfiIdentityStatus.allocationSize(value.`identity`) +
            FfiConverterBoolean.allocationSize(value.`liveSessionAvailable`)
    )

    override fun write(value: FfiInitializationReport, buf: ByteBuffer) {
        FfiConverterTypeFfiIdentityStatus.write(value.`identity`, buf)
        FfiConverterBoolean.write(value.`liveSessionAvailable`, buf)
    }
}




public object FfiConverterTypeFfiLinkedPeerHandshakeReport: FfiConverterRustBuffer<FfiLinkedPeerHandshakeReport> {
    override fun read(buf: ByteBuffer): FfiLinkedPeerHandshakeReport {
        return FfiLinkedPeerHandshakeReport(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiLinkedPeerState.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.read(buf),
        )
    }

    override fun allocationSize(value: FfiLinkedPeerHandshakeReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterTypeFfiLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterULong.allocationSize(value.`generation`) +
            FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.allocationSize(value.`handshakeRole`)
    )

    override fun write(value: FfiLinkedPeerHandshakeReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterTypeFfiLinkedPeerState.write(value.`state`, buf)
        FfiConverterULong.write(value.`generation`, buf)
        FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.write(value.`handshakeRole`, buf)
    }
}




public object FfiConverterTypeFfiLinkedPeerRecord: FfiConverterRustBuffer<FfiLinkedPeerRecord> {
    override fun read(buf: ByteBuffer): FfiLinkedPeerRecord {
        return FfiLinkedPeerRecord(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiLinkedPeerState.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiLinkedPeerRecord): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterTypeFfiLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterOptionalString.allocationSize(value.`lastSyncAt`) +
            FfiConverterOptionalString.allocationSize(value.`lastPrivateReceiveAt`) +
            FfiConverterUInt.allocationSize(value.`failureCount`) +
            FfiConverterOptionalString.allocationSize(value.`localRecoveryAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`localRecoveryMarkerCreatedAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`localRecoveryMarkerLastError`) +
            FfiConverterOptionalString.allocationSize(value.`remoteRecoveryAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`remoteRecoveryMarkerObservedAt`)
    )

    override fun write(value: FfiLinkedPeerRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterTypeFfiLinkedPeerState.write(value.`state`, buf)
        FfiConverterOptionalString.write(value.`lastSyncAt`, buf)
        FfiConverterOptionalString.write(value.`lastPrivateReceiveAt`, buf)
        FfiConverterUInt.write(value.`failureCount`, buf)
        FfiConverterOptionalString.write(value.`localRecoveryAttemptId`, buf)
        FfiConverterOptionalString.write(value.`localRecoveryMarkerCreatedAt`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`localRecoveryMarkerLastError`, buf)
        FfiConverterOptionalString.write(value.`remoteRecoveryAttemptId`, buf)
        FfiConverterOptionalString.write(value.`remoteRecoveryMarkerObservedAt`, buf)
    }
}




public object FfiConverterTypeFfiOutboundPrivateCounterpartySendReport: FfiConverterRustBuffer<FfiOutboundPrivateCounterpartySendReport> {
    override fun read(buf: ByteBuffer): FfiOutboundPrivateCounterpartySendReport {
        return FfiOutboundPrivateCounterpartySendReport(
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiOutboundPrivateCounterpartySendReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.allocationSize(value.`report`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: FfiOutboundPrivateCounterpartySendReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateSendReport.write(value.`report`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiOutboundPrivateSendFailure: FfiConverterRustBuffer<FfiOutboundPrivateSendFailure> {
    override fun read(buf: ByteBuffer): FfiOutboundPrivateSendFailure {
        return FfiOutboundPrivateSendFailure(
            FfiConverterULong.read(buf),
            FfiConverterTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiOutboundPrivateSendFailure): ULong = (
            FfiConverterULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: FfiOutboundPrivateSendFailure, buf: ByteBuffer) {
        FfiConverterULong.write(value.`outboundMessageId`, buf)
        FfiConverterTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiOutboundPrivateSendReport: FfiConverterRustBuffer<FfiOutboundPrivateSendReport> {
    override fun read(buf: ByteBuffer): FfiOutboundPrivateSendReport {
        return FfiOutboundPrivateSendReport(
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceTypeFfiOutboundPrivateSendFailure.read(buf),
            FfiConverterSequenceTypeFfiReservationCleanupFailure.read(buf),
            FfiConverterSequenceTypeFfiRecoveryMarkerPublishFailure.read(buf),
        )
    }

    override fun allocationSize(value: FfiOutboundPrivateSendReport): ULong = (
            FfiConverterSequenceULong.allocationSize(value.`attempted`) +
            FfiConverterSequenceULong.allocationSize(value.`sent`) +
            FfiConverterSequenceTypeFfiOutboundPrivateSendFailure.allocationSize(value.`failed`) +
            FfiConverterSequenceTypeFfiReservationCleanupFailure.allocationSize(value.`reservationCleanupFailures`) +
            FfiConverterSequenceTypeFfiRecoveryMarkerPublishFailure.allocationSize(value.`recoveryMarkerFailures`)
    )

    override fun write(value: FfiOutboundPrivateSendReport, buf: ByteBuffer) {
        FfiConverterSequenceULong.write(value.`attempted`, buf)
        FfiConverterSequenceULong.write(value.`sent`, buf)
        FfiConverterSequenceTypeFfiOutboundPrivateSendFailure.write(value.`failed`, buf)
        FfiConverterSequenceTypeFfiReservationCleanupFailure.write(value.`reservationCleanupFailures`, buf)
        FfiConverterSequenceTypeFfiRecoveryMarkerPublishFailure.write(value.`recoveryMarkerFailures`, buf)
    }
}




public object FfiConverterTypeFfiPaykitBlobRecord: FfiConverterRustBuffer<FfiPaykitBlobRecord> {
    override fun read(buf: ByteBuffer): FfiPaykitBlobRecord {
        return FfiPaykitBlobRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaykitBlobRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`uri`) +
            FfiConverterULong.allocationSize(value.`sizeBytes`) +
            FfiConverterString.allocationSize(value.`updatedAt`)
    )

    override fun write(value: FfiPaykitBlobRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`uri`, buf)
        FfiConverterULong.write(value.`sizeBytes`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
    }
}




public object FfiConverterTypeFfiPaykitProfile: FfiConverterRustBuffer<FfiPaykitProfile> {
    override fun read(buf: ByteBuffer): FfiPaykitProfile {
        return FfiPaykitProfile(
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaykitProfile): ULong = (
            FfiConverterOptionalString.allocationSize(value.`displayName`) +
            FfiConverterOptionalString.allocationSize(value.`imageUri`) +
            FfiConverterOptionalString.allocationSize(value.`extraJson`)
    )

    override fun write(value: FfiPaykitProfile, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`displayName`, buf)
        FfiConverterOptionalString.write(value.`imageUri`, buf)
        FfiConverterOptionalString.write(value.`extraJson`, buf)
    }
}




public object FfiConverterTypeFfiPaykitProfileRecord: FfiConverterRustBuffer<FfiPaykitProfileRecord> {
    override fun read(buf: ByteBuffer): FfiPaykitProfileRecord {
        return FfiPaykitProfileRecord(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaykitProfile.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaykitProfileRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypeFfiPaykitProfile.allocationSize(value.`profile`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`updatedAt`)
    )

    override fun write(value: FfiPaykitProfileRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypeFfiPaykitProfile.write(value.`profile`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
    }
}




public object FfiConverterTypeFfiPaykitSdkConfig: FfiConverterRustBuffer<FfiPaykitSdkConfig> {
    override fun read(buf: ByteBuffer): FfiPaykitSdkConfig {
        return FfiPaykitSdkConfig(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiEndpointManagementScope.read(buf),
            FfiConverterTypeFfiEncryptedLinkRecoveryMarkerPolicy.read(buf),
            FfiConverterTypeFfiPublicContactSharingPolicy.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaykitSdkConfig): ULong = (
            FfiConverterString.allocationSize(value.`profileNamespace`) +
            FfiConverterTypeFfiEndpointManagementScope.allocationSize(value.`endpointManagementScope`) +
            FfiConverterTypeFfiEncryptedLinkRecoveryMarkerPolicy.allocationSize(value.`encryptedLinkRecoveryMarkers`) +
            FfiConverterTypeFfiPublicContactSharingPolicy.allocationSize(value.`publicContactSharing`) +
            FfiConverterULong.allocationSize(value.`peerLinkOperationLeaseTimeoutSecs`) +
            FfiConverterULong.allocationSize(value.`outboundPrivateSendLeaseTimeoutSecs`) +
            FfiConverterULong.allocationSize(value.`outboundPrivateRetryBackoffSecs`)
    )

    override fun write(value: FfiPaykitSdkConfig, buf: ByteBuffer) {
        FfiConverterString.write(value.`profileNamespace`, buf)
        FfiConverterTypeFfiEndpointManagementScope.write(value.`endpointManagementScope`, buf)
        FfiConverterTypeFfiEncryptedLinkRecoveryMarkerPolicy.write(value.`encryptedLinkRecoveryMarkers`, buf)
        FfiConverterTypeFfiPublicContactSharingPolicy.write(value.`publicContactSharing`, buf)
        FfiConverterULong.write(value.`peerLinkOperationLeaseTimeoutSecs`, buf)
        FfiConverterULong.write(value.`outboundPrivateSendLeaseTimeoutSecs`, buf)
        FfiConverterULong.write(value.`outboundPrivateRetryBackoffSecs`, buf)
    }
}




public object FfiConverterTypeFfiPaymentAmountContext: FfiConverterRustBuffer<FfiPaymentAmountContext> {
    override fun read(buf: ByteBuffer): FfiPaymentAmountContext {
        return FfiPaymentAmountContext(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentAmountContext): ULong = (
            FfiConverterString.allocationSize(value.`value`) +
            FfiConverterString.allocationSize(value.`asset`)
    )

    override fun write(value: FfiPaymentAmountContext, buf: ByteBuffer) {
        FfiConverterString.write(value.`value`, buf)
        FfiConverterString.write(value.`asset`, buf)
    }
}




public object FfiConverterTypeFfiPaymentEndpointCandidate: FfiConverterRustBuffer<FfiPaymentEndpointCandidate> {
    override fun read(buf: ByteBuffer): FfiPaymentEndpointCandidate {
        return FfiPaymentEndpointCandidate(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentEndpointSource.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentEndpointCandidate): ULong = (
            FfiConverterString.allocationSize(value.`candidateId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterTypeFfiPaymentEndpointSource.allocationSize(value.`source`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypeFfiPaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: FfiPaymentEndpointCandidate, buf: ByteBuffer) {
        FfiConverterString.write(value.`candidateId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterTypeFfiPaymentEndpointSource.write(value.`source`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypeFfiPaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypeFfiPaymentEndpointReservation: FfiConverterRustBuffer<FfiPaymentEndpointReservation> {
    override fun read(buf: ByteBuffer): FfiPaymentEndpointReservation {
        return FfiPaymentEndpointReservation(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiReceivingDetail.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterTypeFfiReservationAttribution.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentEndpointReservation): ULong = (
            FfiConverterString.allocationSize(value.`reservationId`) +
            FfiConverterTypeFfiReceivingDetail.allocationSize(value.`receivingDetail`) +
            FfiConverterOptionalString.allocationSize(value.`expiresAt`) +
            FfiConverterTypeFfiReservationAttribution.allocationSize(value.`attribution`)
    )

    override fun write(value: FfiPaymentEndpointReservation, buf: ByteBuffer) {
        FfiConverterString.write(value.`reservationId`, buf)
        FfiConverterTypeFfiReceivingDetail.write(value.`receivingDetail`, buf)
        FfiConverterOptionalString.write(value.`expiresAt`, buf)
        FfiConverterTypeFfiReservationAttribution.write(value.`attribution`, buf)
    }
}




public object FfiConverterTypeFfiPaymentEndpointReservationCancellation: FfiConverterRustBuffer<FfiPaymentEndpointReservationCancellation> {
    override fun read(buf: ByteBuffer): FfiPaymentEndpointReservationCancellation {
        return FfiPaymentEndpointReservationCancellation(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiReservationAttribution.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentEndpointReservationCancellation): ULong = (
            FfiConverterString.allocationSize(value.`reservationId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterString.allocationSize(value.`payloadHash`) +
            FfiConverterTypeFfiReservationAttribution.allocationSize(value.`attribution`)
    )

    override fun write(value: FfiPaymentEndpointReservationCancellation, buf: ByteBuffer) {
        FfiConverterString.write(value.`reservationId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterString.write(value.`payloadHash`, buf)
        FfiConverterTypeFfiReservationAttribution.write(value.`attribution`, buf)
    }
}




public object FfiConverterTypeFfiPaymentEndpointSelectionRequest: FfiConverterRustBuffer<FfiPaymentEndpointSelectionRequest> {
    override fun read(buf: ByteBuffer): FfiPaymentEndpointSelectionRequest {
        return FfiPaymentEndpointSelectionRequest(
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPaymentAmountContext.read(buf),
            FfiConverterSequenceTypeFfiPaymentEndpointCandidate.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentEndpointSelectionRequest): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalTypeFfiPaymentAmountContext.allocationSize(value.`amount`) +
            FfiConverterSequenceTypeFfiPaymentEndpointCandidate.allocationSize(value.`candidates`)
    )

    override fun write(value: FfiPaymentEndpointSelectionRequest, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterOptionalTypeFfiPaymentAmountContext.write(value.`amount`, buf)
        FfiConverterSequenceTypeFfiPaymentEndpointCandidate.write(value.`candidates`, buf)
    }
}




public object FfiConverterTypeFfiPaymentTarget: FfiConverterRustBuffer<FfiPaymentTarget> {
    override fun read(buf: ByteBuffer): FfiPaymentTarget {
        return FfiPaymentTarget(
            FfiConverterTypeFfiPaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentTarget): ULong = (
            FfiConverterTypeFfiPaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: FfiPaymentTarget, buf: ByteBuffer) {
        FfiConverterTypeFfiPaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypeFfiPrivatePaymentListEndpoint: FfiConverterRustBuffer<FfiPrivatePaymentListEndpoint> {
    override fun read(buf: ByteBuffer): FfiPrivatePaymentListEndpoint {
        return FfiPrivatePaymentListEndpoint(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: FfiPrivatePaymentListEndpoint): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypeFfiPaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: FfiPrivatePaymentListEndpoint, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypeFfiPaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypeFfiPrivatePaymentListView: FfiConverterRustBuffer<FfiPrivatePaymentListView> {
    override fun read(buf: ByteBuffer): FfiPrivatePaymentListView {
        return FfiPrivatePaymentListView(
            FfiConverterOptionalULong.read(buf),
            FfiConverterSequenceTypeFfiPrivatePaymentListEndpoint.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPrivatePaymentListView): ULong = (
            FfiConverterOptionalULong.allocationSize(value.`latestStreamItemId`) +
            FfiConverterSequenceTypeFfiPrivatePaymentListEndpoint.allocationSize(value.`paymentEndpoints`) +
            FfiConverterOptionalString.allocationSize(value.`lastRefreshAt`)
    )

    override fun write(value: FfiPrivatePaymentListView, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`latestStreamItemId`, buf)
        FfiConverterSequenceTypeFfiPrivatePaymentListEndpoint.write(value.`paymentEndpoints`, buf)
        FfiConverterOptionalString.write(value.`lastRefreshAt`, buf)
    }
}




public object FfiConverterTypeFfiPrivateStreamCounterpartyIntakeReport: FfiConverterRustBuffer<FfiPrivateStreamCounterpartyIntakeReport> {
    override fun read(buf: ByteBuffer): FfiPrivateStreamCounterpartyIntakeReport {
        return FfiPrivateStreamCounterpartyIntakeReport(
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiPrivateStreamCounterpartyIntakeReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.allocationSize(value.`report`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: FfiPrivateStreamCounterpartyIntakeReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.write(value.`report`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiPrivateStreamIntakeReport: FfiConverterRustBuffer<FfiPrivateStreamIntakeReport> {
    override fun read(buf: ByteBuffer): FfiPrivateStreamIntakeReport {
        return FfiPrivateStreamIntakeReport(
            FfiConverterULong.read(buf),
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceTypeFfiEventIdConflict.read(buf),
        )
    }

    override fun allocationSize(value: FfiPrivateStreamIntakeReport): ULong = (
            FfiConverterULong.allocationSize(value.`receiveBatchId`) +
            FfiConverterSequenceULong.allocationSize(value.`streamItemIds`) +
            FfiConverterSequenceTypeFfiEventIdConflict.allocationSize(value.`eventConflicts`)
    )

    override fun write(value: FfiPrivateStreamIntakeReport, buf: ByteBuffer) {
        FfiConverterULong.write(value.`receiveBatchId`, buf)
        FfiConverterSequenceULong.write(value.`streamItemIds`, buf)
        FfiConverterSequenceTypeFfiEventIdConflict.write(value.`eventConflicts`, buf)
    }
}




public object FfiConverterTypeFfiPubkyAuthDetails: FfiConverterRustBuffer<FfiPubkyAuthDetails> {
    override fun read(buf: ByteBuffer): FfiPubkyAuthDetails {
        return FfiPubkyAuthDetails(
            FfiConverterTypeFfiPubkyAuthRequestKind.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyAuthDetails): ULong = (
            FfiConverterTypeFfiPubkyAuthRequestKind.allocationSize(value.`kind`) +
            FfiConverterOptionalString.allocationSize(value.`capabilities`) +
            FfiConverterOptionalString.allocationSize(value.`relayUrl`) +
            FfiConverterOptionalString.allocationSize(value.`homeserverPublicKey`)
    )

    override fun write(value: FfiPubkyAuthDetails, buf: ByteBuffer) {
        FfiConverterTypeFfiPubkyAuthRequestKind.write(value.`kind`, buf)
        FfiConverterOptionalString.write(value.`capabilities`, buf)
        FfiConverterOptionalString.write(value.`relayUrl`, buf)
        FfiConverterOptionalString.write(value.`homeserverPublicKey`, buf)
    }
}




public object FfiConverterTypeFfiPubkyClientConfig: FfiConverterRustBuffer<FfiPubkyClientConfig> {
    override fun read(buf: ByteBuffer): FfiPubkyClientConfig {
        return FfiPubkyClientConfig(
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyClientConfig): ULong = (
            FfiConverterULong.allocationSize(value.`requestTimeoutSecs`)
    )

    override fun write(value: FfiPubkyClientConfig, buf: ByteBuffer) {
        FfiConverterULong.write(value.`requestTimeoutSecs`, buf)
    }
}




public object FfiConverterTypeFfiPubkyProfile: FfiConverterRustBuffer<FfiPubkyProfile> {
    override fun read(buf: ByteBuffer): FfiPubkyProfile {
        return FfiPubkyProfile(
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterSequenceTypeFfiPubkyProfileLink.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyProfile): ULong = (
            FfiConverterString.allocationSize(value.`name`) +
            FfiConverterOptionalString.allocationSize(value.`bio`) +
            FfiConverterOptionalString.allocationSize(value.`image`) +
            FfiConverterSequenceTypeFfiPubkyProfileLink.allocationSize(value.`links`) +
            FfiConverterOptionalString.allocationSize(value.`status`)
    )

    override fun write(value: FfiPubkyProfile, buf: ByteBuffer) {
        FfiConverterString.write(value.`name`, buf)
        FfiConverterOptionalString.write(value.`bio`, buf)
        FfiConverterOptionalString.write(value.`image`, buf)
        FfiConverterSequenceTypeFfiPubkyProfileLink.write(value.`links`, buf)
        FfiConverterOptionalString.write(value.`status`, buf)
    }
}




public object FfiConverterTypeFfiPubkyProfileLink: FfiConverterRustBuffer<FfiPubkyProfileLink> {
    override fun read(buf: ByteBuffer): FfiPubkyProfileLink {
        return FfiPubkyProfileLink(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyProfileLink): ULong = (
            FfiConverterString.allocationSize(value.`title`) +
            FfiConverterString.allocationSize(value.`url`)
    )

    override fun write(value: FfiPubkyProfileLink, buf: ByteBuffer) {
        FfiConverterString.write(value.`title`, buf)
        FfiConverterString.write(value.`url`, buf)
    }
}




public object FfiConverterTypeFfiPubkyProfileRecord: FfiConverterRustBuffer<FfiPubkyProfileRecord> {
    override fun read(buf: ByteBuffer): FfiPubkyProfileRecord {
        return FfiPubkyProfileRecord(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPubkyProfile.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyProfileRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypeFfiPubkyProfile.allocationSize(value.`profile`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`fetchedAt`)
    )

    override fun write(value: FfiPubkyProfileRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypeFfiPubkyProfile.write(value.`profile`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`fetchedAt`, buf)
    }
}




public object FfiConverterTypeFfiPubkyResourceRef: FfiConverterRustBuffer<FfiPubkyResourceRef> {
    override fun read(buf: ByteBuffer): FfiPubkyResourceRef {
        return FfiPubkyResourceRef(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkyResourceRef): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`transportUrl`)
    )

    override fun write(value: FfiPubkyResourceRef, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`transportUrl`, buf)
    }
}




public object FfiConverterTypeFfiPubkySessionBootstrapResult: FfiConverterRustBuffer<FfiPubkySessionBootstrapResult> {
    override fun read(buf: ByteBuffer): FfiPubkySessionBootstrapResult {
        return FfiPubkySessionBootstrapResult(
            FfiConverterTypeFfiPubkySessionAccess.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPubkyIdentityCapability.read(buf),
        )
    }

    override fun allocationSize(value: FfiPubkySessionBootstrapResult): ULong = (
            FfiConverterTypeFfiPubkySessionAccess.allocationSize(value.`sessionAccess`) +
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypeFfiPubkyIdentityCapability.allocationSize(value.`capability`)
    )

    override fun write(value: FfiPubkySessionBootstrapResult, buf: ByteBuffer) {
        FfiConverterTypeFfiPubkySessionAccess.write(value.`sessionAccess`, buf)
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypeFfiPubkyIdentityCapability.write(value.`capability`, buf)
    }
}




public object FfiConverterTypeFfiQueuedPrivateMessage: FfiConverterRustBuffer<FfiQueuedPrivateMessage> {
    override fun read(buf: ByteBuffer): FfiQueuedPrivateMessage {
        return FfiQueuedPrivateMessage(
            FfiConverterULong.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiQueuedPrivateMessage): ULong = (
            FfiConverterULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`kind`) +
            FfiConverterTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`status`) +
            FfiConverterUInt.allocationSize(value.`attemptCount`) +
            FfiConverterString.allocationSize(value.`createdAt`) +
            FfiConverterString.allocationSize(value.`updatedAt`) +
            FfiConverterOptionalString.allocationSize(value.`lastAttemptAt`) +
            FfiConverterOptionalString.allocationSize(value.`sentAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`lastError`)
    )

    override fun write(value: FfiQueuedPrivateMessage, buf: ByteBuffer) {
        FfiConverterULong.write(value.`outboundMessageId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`kind`, buf)
        FfiConverterTypeFfiOutboundPrivateMessageStatus.write(value.`status`, buf)
        FfiConverterUInt.write(value.`attemptCount`, buf)
        FfiConverterString.write(value.`createdAt`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
        FfiConverterOptionalString.write(value.`lastAttemptAt`, buf)
        FfiConverterOptionalString.write(value.`sentAt`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`lastError`, buf)
    }
}




public object FfiConverterTypeFfiReceivingDetail: FfiConverterRustBuffer<FfiReceivingDetail> {
    override fun read(buf: ByteBuffer): FfiReceivingDetail {
        return FfiReceivingDetail(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: FfiReceivingDetail): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypeFfiPaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: FfiReceivingDetail, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypeFfiPaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypeFfiReceivingDetailScope: FfiConverterRustBuffer<FfiReceivingDetailScope> {
    override fun read(buf: ByteBuffer): FfiReceivingDetailScope {
        return FfiReceivingDetailScope(
            FfiConverterTypeFfiReceivingDetailScopeKind.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: FfiReceivingDetailScope): ULong = (
            FfiConverterTypeFfiReceivingDetailScopeKind.allocationSize(value.`kind`) +
            FfiConverterOptionalString.allocationSize(value.`counterparty`)
    )

    override fun write(value: FfiReceivingDetailScope, buf: ByteBuffer) {
        FfiConverterTypeFfiReceivingDetailScopeKind.write(value.`kind`, buf)
        FfiConverterOptionalString.write(value.`counterparty`, buf)
    }
}




public object FfiConverterTypeFfiRecoveryMarkerPublishFailure: FfiConverterRustBuffer<FfiRecoveryMarkerPublishFailure> {
    override fun read(buf: ByteBuffer): FfiRecoveryMarkerPublishFailure {
        return FfiRecoveryMarkerPublishFailure(
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiRecoveryMarkerPublishFailure): ULong = (
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: FfiRecoveryMarkerPublishFailure, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiReservationCleanupFailure: FfiConverterRustBuffer<FfiReservationCleanupFailure> {
    override fun read(buf: ByteBuffer): FfiReservationCleanupFailure {
        return FfiReservationCleanupFailure(
            FfiConverterOptionalString.read(buf),
            FfiConverterTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: FfiReservationCleanupFailure): ULong = (
            FfiConverterOptionalString.allocationSize(value.`reservationId`) +
            FfiConverterTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: FfiReservationCleanupFailure, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`reservationId`, buf)
        FfiConverterTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeFfiResolvedPaymentEndpoint: FfiConverterRustBuffer<FfiResolvedPaymentEndpoint> {
    override fun read(buf: ByteBuffer): FfiResolvedPaymentEndpoint {
        return FfiResolvedPaymentEndpoint(
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentEndpointSource.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeFfiPaymentPayload.read(buf),
            FfiConverterTypeFfiPaymentTarget.read(buf),
        )
    }

    override fun allocationSize(value: FfiResolvedPaymentEndpoint): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterTypeFfiPaymentEndpointSource.allocationSize(value.`source`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypeFfiPaymentPayload.allocationSize(value.`payload`) +
            FfiConverterTypeFfiPaymentTarget.allocationSize(value.`target`)
    )

    override fun write(value: FfiResolvedPaymentEndpoint, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterTypeFfiPaymentEndpointSource.write(value.`source`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypeFfiPaymentPayload.write(value.`payload`, buf)
        FfiConverterTypeFfiPaymentTarget.write(value.`target`, buf)
    }
}




public object FfiConverterTypeFfiRestoreReport: FfiConverterRustBuffer<FfiRestoreReport> {
    override fun read(buf: ByteBuffer): FfiRestoreReport {
        return FfiRestoreReport(
            FfiConverterUInt.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterSequenceString.read(buf),
        )
    }

    override fun allocationSize(value: FfiRestoreReport): ULong = (
            FfiConverterUInt.allocationSize(value.`version`) +
            FfiConverterBoolean.allocationSize(value.`restoredIdentity`) +
            FfiConverterULong.allocationSize(value.`linkedPeers`) +
            FfiConverterULong.allocationSize(value.`contactRecords`) +
            FfiConverterULong.allocationSize(value.`publicEndpointRecords`) +
            FfiConverterULong.allocationSize(value.`paymentEndpointReservations`) +
            FfiConverterULong.allocationSize(value.`encryptedLinkStates`) +
            FfiConverterULong.allocationSize(value.`outboundPrivateMessages`) +
            FfiConverterULong.allocationSize(value.`privateStreamItems`) +
            FfiConverterULong.allocationSize(value.`eventDedupRecords`) +
            FfiConverterULong.allocationSize(value.`receiptAccessRecords`) +
            FfiConverterULong.allocationSize(value.`receiptRecords`) +
            FfiConverterULong.allocationSize(value.`receiptIssuanceRecords`) +
            FfiConverterSequenceString.allocationSize(value.`recoveryRequiredPeers`)
    )

    override fun write(value: FfiRestoreReport, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`version`, buf)
        FfiConverterBoolean.write(value.`restoredIdentity`, buf)
        FfiConverterULong.write(value.`linkedPeers`, buf)
        FfiConverterULong.write(value.`contactRecords`, buf)
        FfiConverterULong.write(value.`publicEndpointRecords`, buf)
        FfiConverterULong.write(value.`paymentEndpointReservations`, buf)
        FfiConverterULong.write(value.`encryptedLinkStates`, buf)
        FfiConverterULong.write(value.`outboundPrivateMessages`, buf)
        FfiConverterULong.write(value.`privateStreamItems`, buf)
        FfiConverterULong.write(value.`eventDedupRecords`, buf)
        FfiConverterULong.write(value.`receiptAccessRecords`, buf)
        FfiConverterULong.write(value.`receiptRecords`, buf)
        FfiConverterULong.write(value.`receiptIssuanceRecords`, buf)
        FfiConverterSequenceString.write(value.`recoveryRequiredPeers`, buf)
    }
}




public object FfiConverterTypeFfiSdkStateBlobSnapshot: FfiConverterRustBuffer<FfiSdkStateBlobSnapshot> {
    override fun read(buf: ByteBuffer): FfiSdkStateBlobSnapshot {
        return FfiSdkStateBlobSnapshot(
            FfiConverterTypeFfiSdkStateBlob.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiSdkStateBlobSnapshot): ULong = (
            FfiConverterTypeFfiSdkStateBlob.allocationSize(value.`blob`) +
            FfiConverterString.allocationSize(value.`revision`)
    )

    override fun write(value: FfiSdkStateBlobSnapshot, buf: ByteBuffer) {
        FfiConverterTypeFfiSdkStateBlob.write(value.`blob`, buf)
        FfiConverterString.write(value.`revision`, buf)
    }
}





public object FfiConverterTypeFfiContactPaymentResolutionPrivateState: FfiConverterRustBuffer<FfiContactPaymentResolutionPrivateState> {
    override fun read(buf: ByteBuffer): FfiContactPaymentResolutionPrivateState = try {
        FfiContactPaymentResolutionPrivateState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiContactPaymentResolutionPrivateState): ULong = 4UL

    override fun write(value: FfiContactPaymentResolutionPrivateState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiContactPaymentResolutionStatus: FfiConverterRustBuffer<FfiContactPaymentResolutionStatus> {
    override fun read(buf: ByteBuffer): FfiContactPaymentResolutionStatus = try {
        FfiContactPaymentResolutionStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiContactPaymentResolutionStatus): ULong = 4UL

    override fun write(value: FfiContactPaymentResolutionStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiContactProfileSource: FfiConverterRustBuffer<FfiContactProfileSource> {
    override fun read(buf: ByteBuffer): FfiContactProfileSource = try {
        FfiContactProfileSource.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiContactProfileSource): ULong = 4UL

    override fun write(value: FfiContactProfileSource, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiEncryptedLinkHandshakeRole: FfiConverterRustBuffer<FfiEncryptedLinkHandshakeRole> {
    override fun read(buf: ByteBuffer): FfiEncryptedLinkHandshakeRole = try {
        FfiEncryptedLinkHandshakeRole.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiEncryptedLinkHandshakeRole): ULong = 4UL

    override fun write(value: FfiEncryptedLinkHandshakeRole, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiEncryptedLinkRecoveryMarkerPolicy: FfiConverterRustBuffer<FfiEncryptedLinkRecoveryMarkerPolicy> {
    override fun read(buf: ByteBuffer): FfiEncryptedLinkRecoveryMarkerPolicy = try {
        FfiEncryptedLinkRecoveryMarkerPolicy.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiEncryptedLinkRecoveryMarkerPolicy): ULong = 4UL

    override fun write(value: FfiEncryptedLinkRecoveryMarkerPolicy, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiEndpointManagementScope: FfiConverterRustBuffer<FfiEndpointManagementScope> {
    override fun read(buf: ByteBuffer): FfiEndpointManagementScope = try {
        FfiEndpointManagementScope.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiEndpointManagementScope): ULong = 4UL

    override fun write(value: FfiEndpointManagementScope, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiLinkedPeerState: FfiConverterRustBuffer<FfiLinkedPeerState> {
    override fun read(buf: ByteBuffer): FfiLinkedPeerState = try {
        FfiLinkedPeerState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiLinkedPeerState): ULong = 4UL

    override fun write(value: FfiLinkedPeerState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiOutboundPrivateMessageStatus: FfiConverterRustBuffer<FfiOutboundPrivateMessageStatus> {
    override fun read(buf: ByteBuffer): FfiOutboundPrivateMessageStatus = try {
        FfiOutboundPrivateMessageStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiOutboundPrivateMessageStatus): ULong = 4UL

    override fun write(value: FfiOutboundPrivateMessageStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiPaymentEndpointSource: FfiConverterRustBuffer<FfiPaymentEndpointSource> {
    override fun read(buf: ByteBuffer): FfiPaymentEndpointSource = try {
        FfiPaymentEndpointSource.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiPaymentEndpointSource): ULong = 4UL

    override fun write(value: FfiPaymentEndpointSource, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiPubkyAuthRequestKind: FfiConverterRustBuffer<FfiPubkyAuthRequestKind> {
    override fun read(buf: ByteBuffer): FfiPubkyAuthRequestKind = try {
        FfiPubkyAuthRequestKind.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiPubkyAuthRequestKind): ULong = 4UL

    override fun write(value: FfiPubkyAuthRequestKind, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiPubkyIdentityCapability: FfiConverterRustBuffer<FfiPubkyIdentityCapability> {
    override fun read(buf: ByteBuffer): FfiPubkyIdentityCapability = try {
        FfiPubkyIdentityCapability.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiPubkyIdentityCapability): ULong = 4UL

    override fun write(value: FfiPubkyIdentityCapability, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiPublicContactSharingPolicy: FfiConverterRustBuffer<FfiPublicContactSharingPolicy> {
    override fun read(buf: ByteBuffer): FfiPublicContactSharingPolicy = try {
        FfiPublicContactSharingPolicy.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiPublicContactSharingPolicy): ULong = 4UL

    override fun write(value: FfiPublicContactSharingPolicy, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiPublicationStatus: FfiConverterRustBuffer<FfiPublicationStatus> {
    override fun read(buf: ByteBuffer): FfiPublicationStatus = try {
        FfiPublicationStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiPublicationStatus): ULong = 4UL

    override fun write(value: FfiPublicationStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeFfiReceivingDetailScopeKind: FfiConverterRustBuffer<FfiReceivingDetailScopeKind> {
    override fun read(buf: ByteBuffer): FfiReceivingDetailScopeKind = try {
        FfiReceivingDetailScopeKind.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: FfiReceivingDetailScopeKind): ULong = 4UL

    override fun write(value: FfiReceivingDetailScopeKind, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




public object PaykitFfiExceptionErrorHandler : UniffiRustCallStatusErrorHandler<PaykitFfiException> {
    override fun lift(errorBuf: RustBufferByValue): PaykitFfiException = FfiConverterTypePaykitFfiError.lift(errorBuf)
}

public object FfiConverterTypePaykitFfiError : FfiConverterRustBuffer<PaykitFfiException> {
    override fun read(buf: ByteBuffer): PaykitFfiException {
        return when (buf.getInt()) {
            1 -> PaykitFfiException.Storage(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            2 -> PaykitFfiException.Identity(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            3 -> PaykitFfiException.Transport(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            4 -> PaykitFfiException.NotFound(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            5 -> PaykitFfiException.Protocol(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            6 -> PaykitFfiException.Policy(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            7 -> PaykitFfiException.PaymentAdapter(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            8 -> PaykitFfiException.RecoveryRequired(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PaykitFfiException): ULong {
        return when (value) {
            is PaykitFfiException.Storage -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.Identity -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.Transport -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.NotFound -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.Protocol -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.Policy -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.PaymentAdapter -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitFfiException.RecoveryRequired -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
        }
    }

    override fun write(value: PaykitFfiException, buf: ByteBuffer) {
        when (value) {
            is PaykitFfiException.Storage -> {
                buf.putInt(1)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.Identity -> {
                buf.putInt(2)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.Transport -> {
                buf.putInt(3)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.NotFound -> {
                buf.putInt(4)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.Protocol -> {
                buf.putInt(5)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.Policy -> {
                buf.putInt(6)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.PaymentAdapter -> {
                buf.putInt(7)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitFfiException.RecoveryRequired -> {
                buf.putInt(8)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}




public object FfiConverterOptionalULong: FfiConverterRustBuffer<kotlin.ULong?> {
    override fun read(buf: ByteBuffer): kotlin.ULong? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterULong.read(buf)
    }

    override fun allocationSize(value: kotlin.ULong?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterULong.allocationSize(value)
        }
    }

    override fun write(value: kotlin.ULong?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterULong.write(value, buf)
        }
    }
}




public object FfiConverterOptionalString: FfiConverterRustBuffer<kotlin.String?> {
    override fun read(buf: ByteBuffer): kotlin.String? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterString.read(buf)
    }

    override fun allocationSize(value: kotlin.String?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterString.allocationSize(value)
        }
    }

    override fun write(value: kotlin.String?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterString.write(value, buf)
        }
    }
}




public object FfiConverterOptionalByteArray: FfiConverterRustBuffer<kotlin.ByteArray?> {
    override fun read(buf: ByteBuffer): kotlin.ByteArray? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterByteArray.read(buf)
    }

    override fun allocationSize(value: kotlin.ByteArray?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterByteArray.allocationSize(value)
        }
    }

    override fun write(value: kotlin.ByteArray?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterByteArray.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivateOperationError: FfiConverterRustBuffer<FfiPrivateOperationError?> {
    override fun read(buf: ByteBuffer): FfiPrivateOperationError? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPrivateOperationError.read(buf)
    }

    override fun allocationSize(value: FfiPrivateOperationError?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPrivateOperationError.allocationSize(value)
        }
    }

    override fun write(value: FfiPrivateOperationError?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPrivateOperationError.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyLocalSecretKey: FfiConverterRustBuffer<FfiPubkyLocalSecretKey?> {
    override fun read(buf: ByteBuffer): FfiPubkyLocalSecretKey? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPubkyLocalSecretKey.read(buf)
    }

    override fun allocationSize(value: FfiPubkyLocalSecretKey?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPubkyLocalSecretKey.allocationSize(value)
        }
    }

    override fun write(value: FfiPubkyLocalSecretKey?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPubkyLocalSecretKey.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkySessionAccess: FfiConverterRustBuffer<FfiPubkySessionAccess?> {
    override fun read(buf: ByteBuffer): FfiPubkySessionAccess? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPubkySessionAccess.read(buf)
    }

    override fun allocationSize(value: FfiPubkySessionAccess?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPubkySessionAccess.allocationSize(value)
        }
    }

    override fun write(value: FfiPubkySessionAccess?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPubkySessionAccess.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiContactProfileResolution: FfiConverterRustBuffer<FfiContactProfileResolution?> {
    override fun read(buf: ByteBuffer): FfiContactProfileResolution? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiContactProfileResolution.read(buf)
    }

    override fun allocationSize(value: FfiContactProfileResolution?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiContactProfileResolution.allocationSize(value)
        }
    }

    override fun write(value: FfiContactProfileResolution?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiContactProfileResolution.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiContactRecord: FfiConverterRustBuffer<FfiContactRecord?> {
    override fun read(buf: ByteBuffer): FfiContactRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiContactRecord.read(buf)
    }

    override fun allocationSize(value: FfiContactRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiContactRecord.allocationSize(value)
        }
    }

    override fun write(value: FfiContactRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiContactRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiEncryptedLinkRecoveryMarkerReport: FfiConverterRustBuffer<FfiEncryptedLinkRecoveryMarkerReport?> {
    override fun read(buf: ByteBuffer): FfiEncryptedLinkRecoveryMarkerReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.read(buf)
    }

    override fun allocationSize(value: FfiEncryptedLinkRecoveryMarkerReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.allocationSize(value)
        }
    }

    override fun write(value: FfiEncryptedLinkRecoveryMarkerReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiEncryptedLinkRecoveryMarkerReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiIdentityStatus: FfiConverterRustBuffer<FfiIdentityStatus?> {
    override fun read(buf: ByteBuffer): FfiIdentityStatus? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiIdentityStatus.read(buf)
    }

    override fun allocationSize(value: FfiIdentityStatus?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiIdentityStatus.allocationSize(value)
        }
    }

    override fun write(value: FfiIdentityStatus?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiIdentityStatus.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiOutboundPrivateSendReport: FfiConverterRustBuffer<FfiOutboundPrivateSendReport?> {
    override fun read(buf: ByteBuffer): FfiOutboundPrivateSendReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiOutboundPrivateSendReport.read(buf)
    }

    override fun allocationSize(value: FfiOutboundPrivateSendReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiOutboundPrivateSendReport.allocationSize(value)
        }
    }

    override fun write(value: FfiOutboundPrivateSendReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiOutboundPrivateSendReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaykitProfile: FfiConverterRustBuffer<FfiPaykitProfile?> {
    override fun read(buf: ByteBuffer): FfiPaykitProfile? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPaykitProfile.read(buf)
    }

    override fun allocationSize(value: FfiPaykitProfile?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPaykitProfile.allocationSize(value)
        }
    }

    override fun write(value: FfiPaykitProfile?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPaykitProfile.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaykitProfileRecord: FfiConverterRustBuffer<FfiPaykitProfileRecord?> {
    override fun read(buf: ByteBuffer): FfiPaykitProfileRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPaykitProfileRecord.read(buf)
    }

    override fun allocationSize(value: FfiPaykitProfileRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPaykitProfileRecord.allocationSize(value)
        }
    }

    override fun write(value: FfiPaykitProfileRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPaykitProfileRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaymentAmountContext: FfiConverterRustBuffer<FfiPaymentAmountContext?> {
    override fun read(buf: ByteBuffer): FfiPaymentAmountContext? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPaymentAmountContext.read(buf)
    }

    override fun allocationSize(value: FfiPaymentAmountContext?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPaymentAmountContext.allocationSize(value)
        }
    }

    override fun write(value: FfiPaymentAmountContext?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPaymentAmountContext.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivatePaymentListView: FfiConverterRustBuffer<FfiPrivatePaymentListView?> {
    override fun read(buf: ByteBuffer): FfiPrivatePaymentListView? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPrivatePaymentListView.read(buf)
    }

    override fun allocationSize(value: FfiPrivatePaymentListView?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPrivatePaymentListView.allocationSize(value)
        }
    }

    override fun write(value: FfiPrivatePaymentListView?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPrivatePaymentListView.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivateStreamIntakeReport: FfiConverterRustBuffer<FfiPrivateStreamIntakeReport?> {
    override fun read(buf: ByteBuffer): FfiPrivateStreamIntakeReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPrivateStreamIntakeReport.read(buf)
    }

    override fun allocationSize(value: FfiPrivateStreamIntakeReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPrivateStreamIntakeReport.allocationSize(value)
        }
    }

    override fun write(value: FfiPrivateStreamIntakeReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPrivateStreamIntakeReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyProfile: FfiConverterRustBuffer<FfiPubkyProfile?> {
    override fun read(buf: ByteBuffer): FfiPubkyProfile? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPubkyProfile.read(buf)
    }

    override fun allocationSize(value: FfiPubkyProfile?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPubkyProfile.allocationSize(value)
        }
    }

    override fun write(value: FfiPubkyProfile?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPubkyProfile.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyProfileRecord: FfiConverterRustBuffer<FfiPubkyProfileRecord?> {
    override fun read(buf: ByteBuffer): FfiPubkyProfileRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPubkyProfileRecord.read(buf)
    }

    override fun allocationSize(value: FfiPubkyProfileRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPubkyProfileRecord.allocationSize(value)
        }
    }

    override fun write(value: FfiPubkyProfileRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPubkyProfileRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiSdkStateBlobSnapshot: FfiConverterRustBuffer<FfiSdkStateBlobSnapshot?> {
    override fun read(buf: ByteBuffer): FfiSdkStateBlobSnapshot? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiSdkStateBlobSnapshot.read(buf)
    }

    override fun allocationSize(value: FfiSdkStateBlobSnapshot?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiSdkStateBlobSnapshot.allocationSize(value)
        }
    }

    override fun write(value: FfiSdkStateBlobSnapshot?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiSdkStateBlobSnapshot.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole: FfiConverterRustBuffer<FfiEncryptedLinkHandshakeRole?> {
    override fun read(buf: ByteBuffer): FfiEncryptedLinkHandshakeRole? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiEncryptedLinkHandshakeRole.read(buf)
    }

    override fun allocationSize(value: FfiEncryptedLinkHandshakeRole?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiEncryptedLinkHandshakeRole.allocationSize(value)
        }
    }

    override fun write(value: FfiEncryptedLinkHandshakeRole?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiEncryptedLinkHandshakeRole.write(value, buf)
        }
    }
}




public object FfiConverterOptionalSequenceTypeFfiPaymentEndpointReservation: FfiConverterRustBuffer<List<FfiPaymentEndpointReservation>?> {
    override fun read(buf: ByteBuffer): List<FfiPaymentEndpointReservation>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceTypeFfiPaymentEndpointReservation.read(buf)
    }

    override fun allocationSize(value: List<FfiPaymentEndpointReservation>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceTypeFfiPaymentEndpointReservation.allocationSize(value)
        }
    }

    override fun write(value: List<FfiPaymentEndpointReservation>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceTypeFfiPaymentEndpointReservation.write(value, buf)
        }
    }
}




public object FfiConverterSequenceULong: FfiConverterRustBuffer<List<kotlin.ULong>> {
    override fun read(buf: ByteBuffer): List<kotlin.ULong> {
        val len = buf.getInt()
        return List<kotlin.ULong>(len) {
            FfiConverterULong.read(buf)
        }
    }

    override fun allocationSize(value: List<kotlin.ULong>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterULong.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<kotlin.ULong>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterULong.write(it, buf)
        }
    }
}




public object FfiConverterSequenceString: FfiConverterRustBuffer<List<kotlin.String>> {
    override fun read(buf: ByteBuffer): List<kotlin.String> {
        val len = buf.getInt()
        return List<kotlin.String>(len) {
            FfiConverterString.read(buf)
        }
    }

    override fun allocationSize(value: List<kotlin.String>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterString.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<kotlin.String>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterString.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiContactRecord: FfiConverterRustBuffer<List<FfiContactRecord>> {
    override fun read(buf: ByteBuffer): List<FfiContactRecord> {
        val len = buf.getInt()
        return List<FfiContactRecord>(len) {
            FfiConverterTypeFfiContactRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiContactRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiContactRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiContactRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiContactRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiEndpointSyncChange: FfiConverterRustBuffer<List<FfiEndpointSyncChange>> {
    override fun read(buf: ByteBuffer): List<FfiEndpointSyncChange> {
        val len = buf.getInt()
        return List<FfiEndpointSyncChange>(len) {
            FfiConverterTypeFfiEndpointSyncChange.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiEndpointSyncChange>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiEndpointSyncChange.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiEndpointSyncChange>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiEndpointSyncChange.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiEventIdConflict: FfiConverterRustBuffer<List<FfiEventIdConflict>> {
    override fun read(buf: ByteBuffer): List<FfiEventIdConflict> {
        val len = buf.getInt()
        return List<FfiEventIdConflict>(len) {
            FfiConverterTypeFfiEventIdConflict.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiEventIdConflict>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiEventIdConflict.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiEventIdConflict>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiEventIdConflict.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiLinkedPeerRecord: FfiConverterRustBuffer<List<FfiLinkedPeerRecord>> {
    override fun read(buf: ByteBuffer): List<FfiLinkedPeerRecord> {
        val len = buf.getInt()
        return List<FfiLinkedPeerRecord>(len) {
            FfiConverterTypeFfiLinkedPeerRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiLinkedPeerRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiLinkedPeerRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiLinkedPeerRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiLinkedPeerRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiOutboundPrivateCounterpartySendReport: FfiConverterRustBuffer<List<FfiOutboundPrivateCounterpartySendReport>> {
    override fun read(buf: ByteBuffer): List<FfiOutboundPrivateCounterpartySendReport> {
        val len = buf.getInt()
        return List<FfiOutboundPrivateCounterpartySendReport>(len) {
            FfiConverterTypeFfiOutboundPrivateCounterpartySendReport.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiOutboundPrivateCounterpartySendReport>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiOutboundPrivateCounterpartySendReport.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiOutboundPrivateCounterpartySendReport>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiOutboundPrivateCounterpartySendReport.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiOutboundPrivateSendFailure: FfiConverterRustBuffer<List<FfiOutboundPrivateSendFailure>> {
    override fun read(buf: ByteBuffer): List<FfiOutboundPrivateSendFailure> {
        val len = buf.getInt()
        return List<FfiOutboundPrivateSendFailure>(len) {
            FfiConverterTypeFfiOutboundPrivateSendFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiOutboundPrivateSendFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiOutboundPrivateSendFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiOutboundPrivateSendFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiOutboundPrivateSendFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPaymentEndpointCandidate: FfiConverterRustBuffer<List<FfiPaymentEndpointCandidate>> {
    override fun read(buf: ByteBuffer): List<FfiPaymentEndpointCandidate> {
        val len = buf.getInt()
        return List<FfiPaymentEndpointCandidate>(len) {
            FfiConverterTypeFfiPaymentEndpointCandidate.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPaymentEndpointCandidate>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPaymentEndpointCandidate.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPaymentEndpointCandidate>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPaymentEndpointCandidate.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPaymentEndpointReservation: FfiConverterRustBuffer<List<FfiPaymentEndpointReservation>> {
    override fun read(buf: ByteBuffer): List<FfiPaymentEndpointReservation> {
        val len = buf.getInt()
        return List<FfiPaymentEndpointReservation>(len) {
            FfiConverterTypeFfiPaymentEndpointReservation.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPaymentEndpointReservation>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPaymentEndpointReservation.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPaymentEndpointReservation>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPaymentEndpointReservation.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPrivatePaymentListEndpoint: FfiConverterRustBuffer<List<FfiPrivatePaymentListEndpoint>> {
    override fun read(buf: ByteBuffer): List<FfiPrivatePaymentListEndpoint> {
        val len = buf.getInt()
        return List<FfiPrivatePaymentListEndpoint>(len) {
            FfiConverterTypeFfiPrivatePaymentListEndpoint.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPrivatePaymentListEndpoint>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPrivatePaymentListEndpoint.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPrivatePaymentListEndpoint>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPrivatePaymentListEndpoint.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPrivateStreamCounterpartyIntakeReport: FfiConverterRustBuffer<List<FfiPrivateStreamCounterpartyIntakeReport>> {
    override fun read(buf: ByteBuffer): List<FfiPrivateStreamCounterpartyIntakeReport> {
        val len = buf.getInt()
        return List<FfiPrivateStreamCounterpartyIntakeReport>(len) {
            FfiConverterTypeFfiPrivateStreamCounterpartyIntakeReport.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPrivateStreamCounterpartyIntakeReport>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPrivateStreamCounterpartyIntakeReport.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPrivateStreamCounterpartyIntakeReport>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPrivateStreamCounterpartyIntakeReport.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPubkyProfileLink: FfiConverterRustBuffer<List<FfiPubkyProfileLink>> {
    override fun read(buf: ByteBuffer): List<FfiPubkyProfileLink> {
        val len = buf.getInt()
        return List<FfiPubkyProfileLink>(len) {
            FfiConverterTypeFfiPubkyProfileLink.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPubkyProfileLink>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPubkyProfileLink.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPubkyProfileLink>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPubkyProfileLink.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiReceivingDetail: FfiConverterRustBuffer<List<FfiReceivingDetail>> {
    override fun read(buf: ByteBuffer): List<FfiReceivingDetail> {
        val len = buf.getInt()
        return List<FfiReceivingDetail>(len) {
            FfiConverterTypeFfiReceivingDetail.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiReceivingDetail>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiReceivingDetail.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiReceivingDetail>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiReceivingDetail.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiRecoveryMarkerPublishFailure: FfiConverterRustBuffer<List<FfiRecoveryMarkerPublishFailure>> {
    override fun read(buf: ByteBuffer): List<FfiRecoveryMarkerPublishFailure> {
        val len = buf.getInt()
        return List<FfiRecoveryMarkerPublishFailure>(len) {
            FfiConverterTypeFfiRecoveryMarkerPublishFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiRecoveryMarkerPublishFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiRecoveryMarkerPublishFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiRecoveryMarkerPublishFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiRecoveryMarkerPublishFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiReservationCleanupFailure: FfiConverterRustBuffer<List<FfiReservationCleanupFailure>> {
    override fun read(buf: ByteBuffer): List<FfiReservationCleanupFailure> {
        val len = buf.getInt()
        return List<FfiReservationCleanupFailure>(len) {
            FfiConverterTypeFfiReservationCleanupFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiReservationCleanupFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiReservationCleanupFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiReservationCleanupFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiReservationCleanupFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiResolvedPaymentEndpoint: FfiConverterRustBuffer<List<FfiResolvedPaymentEndpoint>> {
    override fun read(buf: ByteBuffer): List<FfiResolvedPaymentEndpoint> {
        val len = buf.getInt()
        return List<FfiResolvedPaymentEndpoint>(len) {
            FfiConverterTypeFfiResolvedPaymentEndpoint.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiResolvedPaymentEndpoint>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiResolvedPaymentEndpoint.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiResolvedPaymentEndpoint>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiResolvedPaymentEndpoint.write(it, buf)
        }
    }
}



public object FfiConverterMapStringString: FfiConverterRustBuffer<Map<kotlin.String, kotlin.String>> {
    override fun read(buf: ByteBuffer): Map<kotlin.String, kotlin.String> {
        val len = buf.getInt()
        return buildMap<kotlin.String, kotlin.String>(len) {
            repeat(len) {
                val k = FfiConverterString.read(buf)
                val v = FfiConverterString.read(buf)
                this[k] = v
            }
        }
    }

    override fun allocationSize(value: Map<kotlin.String, kotlin.String>): ULong {
        val spaceForMapSize = 4UL
        val spaceForChildren = value.entries.sumOf { (k, v) ->
            FfiConverterString.allocationSize(k) +
            FfiConverterString.allocationSize(v)
        }
        return spaceForMapSize + spaceForChildren
    }

    override fun write(value: Map<kotlin.String, kotlin.String>, buf: ByteBuffer) {
        buf.putInt(value.size)
        // The parens on `(k, v)` here ensure we're calling the right method,
        // which is important for compatibility with older android devices.
        // Ref https://blog.danlew.net/2017/03/16/kotlin-puzzler-whose-line-is-it-anyways/
        value.forEach { (k, v) ->
            FfiConverterString.write(k, buf)
            FfiConverterString.write(v, buf)
        }
    }
}












/**
 * Return the core Paykit session capabilities.
 */
public fun `coreSessionCapabilities`(): kotlin.String {
    return FfiConverterString.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_core_session_capabilities(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the default SDK configuration.
 */
public fun `defaultConfig`(): FfiPaykitSdkConfig {
    return FfiConverterTypeFfiPaykitSdkConfig.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_default_config(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the default Pubky client configuration.
 */
public fun `defaultPubkyClientConfig`(): FfiPubkyClientConfig {
    return FfiConverterTypeFfiPubkyClientConfig.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_default_pubky_client_config(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Derive a local Pubky secret key from a 64-byte wallet seed.
 */
@Throws(PaykitFfiException::class)
public fun `derivePubkySecretKey`(`seed`: kotlin.ByteArray, `runtimeLabel`: kotlin.String): FfiPubkyLocalSecretKey {
    return FfiConverterTypeFfiPubkyLocalSecretKey.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_derive_pubky_secret_key(
            FfiConverterByteArray.lower(`seed`),
            FfiConverterString.lower(`runtimeLabel`),
            uniffiRustCallStatus,
        )
    }!!)
}

/**
 * Parse an auth deep link into public request details.
 */
@Throws(PaykitFfiException::class)
public fun `parsePubkyAuthUrl`(`authUrl`: kotlin.String): FfiPubkyAuthDetails {
    return FfiConverterTypeFfiPubkyAuthDetails.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_parse_pubky_auth_url(
            FfiConverterString.lower(`authUrl`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Parse a `pubky://<public-key>/<path>` resource into stable parts.
 */
@Throws(PaykitFfiException::class)
public fun `parsePubkyResource`(`uri`: kotlin.String): FfiPubkyResourceRef {
    return FfiConverterTypeFfiPubkyResourceRef.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_parse_pubky_resource(
            FfiConverterString.lower(`uri`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the Pubky public key for a local secret key.
 */
@Throws(PaykitFfiException::class)
public fun `pubkyPublicKeyFromSecret`(`localSecretKey`: FfiPubkyLocalSecretKey): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_pubky_public_key_from_secret(
            FfiConverterTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return Pubky capabilities required by this SDK configuration.
 */
@Throws(PaykitFfiException::class)
public fun `requiredSessionCapabilities`(`config`: FfiPaykitSdkConfig): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_required_session_capabilities(
            FfiConverterTypeFfiPaykitSdkConfig.lower(`config`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Resolve a Pubky URI into the transport URL used by Pubky storage.
 */
@Throws(PaykitFfiException::class)
public fun `resolvePubkyUrl`(`uri`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_resolve_pubky_url(
            FfiConverterString.lower(`uri`),
            uniffiRustCallStatus,
        )
    })
}


// Async support

internal const val UNIFFI_RUST_FUTURE_POLL_READY = 0.toByte()
internal const val UNIFFI_RUST_FUTURE_POLL_MAYBE_READY = 1.toByte()

internal val uniffiContinuationHandleMap = UniffiHandleMap<CancellableContinuation<Byte>>()

// FFI type for Rust future continuations
internal suspend fun<T, F, E: kotlin.Exception> uniffiRustCallAsync(
    rustFuture: Long,
    pollFunc: (Long, UniffiRustFutureContinuationCallback, Long) -> Unit,
    completeFunc: (Long, UniffiRustCallStatus) -> F,
    freeFunc: (Long) -> Unit,
    cancelFunc: (Long) -> Unit,
    liftFunc: (F) -> T,
    errorHandler: UniffiRustCallStatusErrorHandler<E>
): T {
    return withContext(Dispatchers.IO) {
        try {
            do {
                val pollResult = suspendCancellableCoroutine<Byte> { continuation ->
                    val handle = uniffiContinuationHandleMap.insert(continuation)
                    continuation.invokeOnCancellation {
                        cancelFunc(rustFuture)
                    }
                    pollFunc(
                        rustFuture,
                        uniffiRustFutureContinuationCallbackCallback,
                        handle
                    )
                }
            } while (pollResult != UNIFFI_RUST_FUTURE_POLL_READY);

            return@withContext liftFunc(
                uniffiRustCallWithError(errorHandler) { status -> completeFunc(rustFuture, status) }
            )
        } finally {
            freeFunc(rustFuture)
        }
    }
}

internal object uniffiRustFutureContinuationCallbackCallback: UniffiRustFutureContinuationCallback {
    override fun callback(data: Long, pollResult: Byte) {
        uniffiContinuationHandleMap.remove(data).resume(pollResult)
    }
}
