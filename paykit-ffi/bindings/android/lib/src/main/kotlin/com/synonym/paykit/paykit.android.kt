

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
    public fun callback(`uniffiHandle`: Long,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`counterparty`: RustBufferByValue,`counterpartyReceiverPath`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`counterparty`: RustBufferByValue,`counterpartyReceiverPath`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`cancellation`: RustBufferByValue,`uniffiOutReturn`: Pointer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`request`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod5: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`endpoint`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod6: com.sun.jna.Callback {
    public fun callback(`uniffiHandle`: Long,`request`: RustBufferByValue,`uniffiOutReturn`: RustBuffer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod7: com.sun.jna.Callback {
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
@Structure.FieldOrder("currentPublicReceivingDetails", "currentPrivateReceivingDetails", "reservePrivateReceivingDetails", "cancelPrivateReceivingDetailReservation", "selectPublicPaymentEndpointIds", "buildPublicPaymentTarget", "selectPrivatePaymentEndpointIds", "buildPrivatePaymentTarget", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterStruct(
    @JvmField public var `currentPublicReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0?,
    @JvmField public var `currentPrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1?,
    @JvmField public var `reservePrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2?,
    @JvmField public var `cancelPrivateReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3?,
    @JvmField public var `selectPublicPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4?,
    @JvmField public var `buildPublicPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod5?,
    @JvmField public var `selectPrivatePaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod6?,
    @JvmField public var `buildPrivatePaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod7?,
    @JvmField public var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `currentPublicReceivingDetails` = null,

        `currentPrivateReceivingDetails` = null,

        `reservePrivateReceivingDetails` = null,

        `cancelPrivateReceivingDetailReservation` = null,

        `selectPublicPaymentEndpointIds` = null,

        `buildPublicPaymentTarget` = null,

        `selectPrivatePaymentEndpointIds` = null,

        `buildPrivatePaymentTarget` = null,

        `uniffiFree` = null,

    )

    internal class UniffiByValue(
        `currentPublicReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0?,
        `currentPrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1?,
        `reservePrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2?,
        `cancelPrivateReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3?,
        `selectPublicPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4?,
        `buildPublicPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod5?,
        `selectPrivatePaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod6?,
        `buildPrivatePaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod7?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter(`currentPublicReceivingDetails`,`currentPrivateReceivingDetails`,`reservePrivateReceivingDetails`,`cancelPrivateReceivingDetailReservation`,`selectPublicPaymentEndpointIds`,`buildPublicPaymentTarget`,`selectPrivatePaymentEndpointIds`,`buildPrivatePaymentTarget`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter = UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterStruct

internal fun UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter) {
    `currentPublicReceivingDetails` = other.`currentPublicReceivingDetails`
    `currentPrivateReceivingDetails` = other.`currentPrivateReceivingDetails`
    `reservePrivateReceivingDetails` = other.`reservePrivateReceivingDetails`
    `cancelPrivateReceivingDetailReservation` = other.`cancelPrivateReceivingDetailReservation`
    `selectPublicPaymentEndpointIds` = other.`selectPublicPaymentEndpointIds`
    `buildPublicPaymentTarget` = other.`buildPublicPaymentTarget`
    `selectPrivatePaymentEndpointIds` = other.`selectPrivatePaymentEndpointIds`
    `buildPrivatePaymentTarget` = other.`buildPrivatePaymentTarget`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkPaymentAdapterUniffiByValue) {
    `currentPublicReceivingDetails` = other.`currentPublicReceivingDetails`
    `currentPrivateReceivingDetails` = other.`currentPrivateReceivingDetails`
    `reservePrivateReceivingDetails` = other.`reservePrivateReceivingDetails`
    `cancelPrivateReceivingDetailReservation` = other.`cancelPrivateReceivingDetailReservation`
    `selectPublicPaymentEndpointIds` = other.`selectPublicPaymentEndpointIds`
    `buildPublicPaymentTarget` = other.`buildPublicPaymentTarget`
    `selectPrivatePaymentEndpointIds` = other.`selectPrivatePaymentEndpointIds`
    `buildPrivatePaymentTarget` = other.`buildPrivatePaymentTarget`
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
@Structure.FieldOrder("loadStateBlob", "saveStateBlobAtomically", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreStruct(
    @JvmField public var `loadStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0?,
    @JvmField public var `saveStateBlobAtomically`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1?,
    @JvmField public var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    internal constructor(): this(

        `loadStateBlob` = null,

        `saveStateBlobAtomically` = null,

        `uniffiFree` = null,

    )

    internal class UniffiByValue(
        `loadStateBlob`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod0?,
        `saveStateBlobAtomically`: UniffiCallbackInterfaceFfiSdkStateBlobStoreMethod1?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceFfiSdkStateBlobStore(`loadStateBlob`,`saveStateBlobAtomically`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceFfiSdkStateBlobStore = UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreStruct

internal fun UniffiVTableCallbackInterfaceFfiSdkStateBlobStore.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkStateBlobStore) {
    `loadStateBlob` = other.`loadStateBlob`
    `saveStateBlobAtomically` = other.`saveStateBlobAtomically`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceFfiSdkStateBlobStore.uniffiSetValue(other: UniffiVTableCallbackInterfaceFfiSdkStateBlobStoreUniffiByValue) {
    `loadStateBlob` = other.`loadStateBlob`
    `saveStateBlobAtomically` = other.`saveStateBlobAtomically`
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
        if (uniffi_paykit_checksum_func_decode_sdk_state_blob_snapshot() != 4823.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_default_config() != 58310.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_default_pubky_client_config() != 12841.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_encode_sdk_state_blob_snapshot() != 49508.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_generate_receipt_id() != 34487.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_normalize_pubky_public_key() != 1980.toShort()) {
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
        if (uniffi_paykit_checksum_func_pubky_secret_key_from_bip39_mnemonic() != 59779.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_pubky_secret_key_from_bip39_seed() != 48251.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_raw_pubky_public_key() != 57096.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_redacted_pubky_public_key() != 54739.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_required_session_capabilities() != 62729.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_resolve_pubky_url() != 12085.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_active_from() != 56693.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_allowed_payment_endpoint_identifiers() != 63441.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_asset() != 61516.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_expires_at() != 42679.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_lifetime_amount_limit() != 50133.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_per_payment_amount() != 64508.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiallowanceterms_period_limits() != 52774.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_accept_allowance() != 19038.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_accept_link_with_peer() != 24950.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_accept_payment_request() != 859.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_actionable_received_payment_requests() != 10342.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_active_recurring_payment_requests() != 2902.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_advance_link_handshake() != 21645.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_block_peer() != 26542.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_cancel_payment_request() != 8092.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_clear_private_payment_list() != 47172.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_clear_private_payment_list_and_process_outbound() != 23510.toShort()) {
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
        if (uniffi_paykit_checksum_method_ffipaykitsdk_current_private_payment_list() != 42695.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_current_profile() != 37415.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_blob() != 43993.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_profile() != 14091.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_encrypted_link_recovery_marker_status() != 64910.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_end_allowance() != 27377.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list() != 16764.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list_with_receiving_details() != 58275.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_ensure_link_with_peer() != 15662.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_state() != 29122.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_string() != 15207.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_fetch_paykit_profile() != 36027.toShort()) {
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
        if (uniffi_paykit_checksum_method_ffipaykitsdk_get_allowance() != 44953.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_identity_status() != 8559.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_initialize() != 60774.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_initiate_link_with_peer() != 39251.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_issue_receipt() != 65469.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_issued_receipts() != 50665.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_issued_receipts_to() != 9366.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_linked_peers() != 57246.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_list_allowances() != 59062.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_list_payment_requests() != 43354.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_observe_encrypted_link_recovery_marker() != 54332.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_paykit_receiver_marker() != 47993.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_paykit_receiver_paths() != 12509.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_payment_requests() != 9060.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_payment_requests_with() != 35782.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_pending_outbound_private_counterparties() != 32211.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_prepare_and_resolve_private_contact_payment() != 46826.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_prepare_receipt_issuance() != 38644.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_process_outbound_private_messages() != 37957.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_process_pending_private_messages() != 56244.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_process_receipt_issuance() != 18672.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_propose_allowance() != 8566.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_propose_payment_request() != 35762.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_encrypted_link_recovery_marker() != 60401.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_blob() != 48358.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_profile() != 19918.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_receiver_marker() != 26480.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_publish_public_contact() != 54711.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access() != 27958.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access_from() != 62798.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access_records() != 13671.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipt_issuance_records() != 3870.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipt_records() != 2833.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipts() != 46308.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receipts_from() != 56234.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages() != 554.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages_from_linked_peers() != 15229.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_received_payment_requests_from() != 14.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_refresh_contact_paykit_profile() != 26474.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_reject_allowance() != 56162.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_reject_payment_request() != 14619.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_contact() != 19304.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_encrypted_link_recovery_marker() != 54279.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_paykit_receiver_marker() != 64082.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_remove_public_contact() != 4060.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_profile() != 57380.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_private_contact_payment() != 52408.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_profile() != 46263.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_resolve_public_contact_payment() != 19789.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_state() != 30409.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_string() != 23617.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_retrieve_receipt() != 4261.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_save_contact() != 7511.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sign_out() != 28715.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_state_revision() != 21336.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_submit_payment_proof() != 13468.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_contact_private_payment_lists() != 14363.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_contact_private_payment_lists_and_process_outbound() != 36895.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_private_payment_lists_with_reservations_and_process_outbound() != 7347.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_contact_markers() != 39954.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints() != 41929.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints_with_receiving_details() != 37396.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_unblock_peer() != 6518.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaykitsdk_upload_profile_avatar() != 49965.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaymentpayload_export_text() != 53824.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipaymentreference_export_text() != 10144.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffiprivatejsonobject_export_text() != 41754.toShort()) {
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
        if (uniffi_paykit_checksum_method_ffipubkyauthrequest_complete() != 51216.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkylocalsecretkey_export_bytes() != 58726.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionaccess_export_local_secret_key() != 61849.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionaccess_export_receiver_noise_secret_key() != 4431.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionaccess_export_session_secret() != 4660.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth() != 21644.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth_with_companion_claim() != 6650.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_import_session() != 27640.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_resume_auth() != 45596.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_in() != 60739.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_sign_up() != 25951.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_in_auth() != 47023.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffipubkysessionbootstrap_start_sign_up_auth() != 45811.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffireceivernoisesecretkey_export_bytes() != 50277.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffireservationattribution_export_fields() != 11904.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkbackupblob_export_bytes() != 43352.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_public_receiving_details() != 46945.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_private_receiving_details() != 8016.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_reserve_private_receiving_details() != 32427.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_cancel_private_receiving_detail_reservation() != 29790.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_public_payment_endpoint_ids() != 47644.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_public_payment_target() != 47953.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_private_payment_endpoint_ids() != 60225.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_private_payment_target() != 8044.toShort()) {
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
        if (uniffi_paykit_checksum_constructor_ffiallowanceterms_new() != 23450.toShort()) {
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
        if (uniffi_paykit_checksum_constructor_ffipaymentreference_new() != 26530.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffiprivatejsonobject_new() != 62907.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkylocalsecretkey_new() != 13295.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionaccess_new() != 5869.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_new() != 44998.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffipubkysessionbootstrap_with_pubky_client_config() != 35417.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffireceivernoisesecretkey_new() != 34247.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_constructor_ffireceivernoisesecretkey_random() != 54931.toShort()) {
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
    external fun uniffi_paykit_checksum_func_decode_sdk_state_blob_snapshot(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_default_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_default_pubky_client_config(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_encode_sdk_state_blob_snapshot(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_generate_receipt_id(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_normalize_pubky_public_key(
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
    external fun uniffi_paykit_checksum_func_pubky_secret_key_from_bip39_mnemonic(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_pubky_secret_key_from_bip39_seed(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_raw_pubky_public_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_redacted_pubky_public_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_required_session_capabilities(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_resolve_pubky_url(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_active_from(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_allowed_payment_endpoint_identifiers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_asset(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_expires_at(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_lifetime_amount_limit(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_per_payment_amount(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiallowanceterms_period_limits(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_accept_allowance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_accept_link_with_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_accept_payment_request(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_actionable_received_payment_requests(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_active_recurring_payment_requests(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_advance_link_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_block_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_cancel_payment_request(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_clear_private_payment_list(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_clear_private_payment_list_and_process_outbound(
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
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_current_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_blob(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_delete_paykit_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_end_allowance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_enqueue_private_payment_list_with_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_ensure_link_with_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_state(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_export_backup_string(
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
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_get_allowance(
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
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_issue_receipt(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_issued_receipts(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_issued_receipts_to(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_linked_peers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_list_allowances(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_list_payment_requests(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_paykit_receiver_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_paykit_receiver_paths(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_payment_requests(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_payment_requests_with(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_pending_outbound_private_counterparties(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_prepare_and_resolve_private_contact_payment(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_prepare_receipt_issuance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_process_outbound_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_process_pending_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_process_receipt_issuance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_propose_allowance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_propose_payment_request(
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
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_paykit_receiver_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_publish_public_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access_from(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipt_access_records(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipt_issuance_records(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipt_records(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipts(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receipts_from(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_receive_private_messages_from_linked_peers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_received_payment_requests_from(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_refresh_contact_paykit_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_reject_allowance(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_reject_payment_request(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_encrypted_link_recovery_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_paykit_receiver_marker(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_remove_public_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_contact_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_private_contact_payment(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_profile(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_resolve_public_contact_payment(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_state(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_restore_backup_string(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_retrieve_receipt(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_save_contact(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sign_out(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_state_revision(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_submit_payment_proof(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_contact_private_payment_lists(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_contact_private_payment_lists_and_process_outbound(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_private_payment_lists_with_reservations_and_process_outbound(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_contact_markers(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_sync_public_endpoints_with_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_unblock_peer(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaykitsdk_upload_profile_avatar(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaymentpayload_export_text(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipaymentreference_export_text(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffiprivatejsonobject_export_text(
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
    external fun uniffi_paykit_checksum_method_ffipubkysessionaccess_export_receiver_noise_secret_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionaccess_export_session_secret(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffipubkysessionbootstrap_approve_auth_with_companion_claim(
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
    external fun uniffi_paykit_checksum_method_ffireceivernoisesecretkey_export_bytes(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffireservationattribution_export_fields(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkbackupblob_export_bytes(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_public_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_current_private_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_reserve_private_receiving_details(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_cancel_private_receiving_detail_reservation(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_public_payment_endpoint_ids(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_public_payment_target(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_select_private_payment_endpoint_ids(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_method_ffisdkpaymentadapter_build_private_payment_target(
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
    external fun uniffi_paykit_checksum_constructor_ffiallowanceterms_new(
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
    external fun uniffi_paykit_checksum_constructor_ffipaymentreference_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffiprivatejsonobject_new(
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
    external fun uniffi_paykit_checksum_constructor_ffireceivernoisesecretkey_new(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_constructor_ffireceivernoisesecretkey_random(
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
    external fun uniffi_paykit_fn_clone_ffiallowanceterms(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffiallowanceterms(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffiallowanceterms_new(
        `asset`: RustBufferByValue,
        `perPaymentAmount`: RustBufferByValue,
        `periodLimits`: RustBufferByValue,
        `lifetimeAmountLimit`: RustBufferByValue,
        `activeFrom`: RustBufferByValue,
        `expiresAt`: RustBufferByValue,
        `allowedPaymentEndpointIdentifiers`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_active_from(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_allowed_payment_endpoint_identifiers(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_asset(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_expires_at(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_lifetime_amount_limit(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_per_payment_amount(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiallowanceterms_period_limits(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
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
    external fun uniffi_paykit_fn_method_ffipaykitsdk_accept_allowance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `allowanceId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_accept_link_with_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_accept_payment_request(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `paymentRequestId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_actionable_received_payment_requests(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_active_recurring_payment_requests(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_advance_link_handshake(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_block_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_cancel_payment_request(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `paymentRequestId`: RustBufferByValue,
        `reason`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_clear_private_payment_list(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_clear_private_payment_list_and_process_outbound(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
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
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_current_profile(
        `ptr`: Pointer?,
        `allowPubkyProfileFallback`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_delete_paykit_blob(
        `ptr`: Pointer?,
        `uriOrPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_delete_paykit_profile(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_end_allowance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `allowanceId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list_with_receiving_details(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `receivingDetails`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_ensure_link_with_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `maxAdvanceSteps`: Int,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_export_backup_state(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_export_backup_string(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_fetch_paykit_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
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
    external fun uniffi_paykit_fn_method_ffipaykitsdk_get_allowance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `allowanceId`: RustBufferByValue,
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
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_issue_receipt(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `draft`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_issued_receipts(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_issued_receipts_to(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_linked_peers(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_list_allowances(
        `ptr`: Pointer?,
        `filter`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_list_payment_requests(
        `ptr`: Pointer?,
        `filter`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_paykit_receiver_marker(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_paykit_receiver_paths(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_payment_requests(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_payment_requests_with(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_pending_outbound_private_counterparties(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_prepare_and_resolve_private_contact_payment(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `amount`: RustBufferByValue,
        `afterPrivatePaymentListVersion`: RustBufferByValue,
        `maxAdvanceSteps`: Int,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_prepare_receipt_issuance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `draft`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_process_outbound_private_messages(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_process_pending_private_messages(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_process_receipt_issuance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `receiptId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_propose_allowance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `localRole`: RustBufferByValue,
        `terms`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_propose_payment_request(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `terms`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_encrypted_link_recovery_marker(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
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
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_receiver_marker(
        `ptr`: Pointer?,
        `capabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_publish_public_contact(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipt_access(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipt_access_from(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipt_access_records(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipt_issuance_records(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipt_records(
        `ptr`: Pointer?,
        `issuer`: RustBufferByValue,
        `issuerReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipts(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receipts_from(
        `ptr`: Pointer?,
        `issuer`: RustBufferByValue,
        `issuerReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages_from_linked_peers(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_received_payment_requests_from(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_refresh_contact_paykit_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_reject_allowance(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `allowanceId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_reject_payment_request(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `paymentRequestId`: RustBufferByValue,
        `reason`: RustBufferByValue,
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
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_remove_paykit_receiver_marker(
        `ptr`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_remove_public_contact(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
        `allowPubkyProfileFallback`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_private_contact_payment(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `amount`: RustBufferByValue,
        `afterPrivatePaymentListVersion`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_profile(
        `ptr`: Pointer?,
        `publicKey`: RustBufferByValue,
        `receiverPath`: RustBufferByValue,
        `allowPubkyProfileFallback`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_resolve_public_contact_payment(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `amount`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_state(
        `ptr`: Pointer?,
        `backup`: Pointer?,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_string(
        `ptr`: Pointer?,
        `backup`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_retrieve_receipt(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `receiptId`: RustBufferByValue,
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
    external fun uniffi_paykit_fn_method_ffipaykitsdk_state_revision(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_submit_payment_proof(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        `paymentRequestId`: RustBufferByValue,
        `proof`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_contact_private_payment_lists(
        `ptr`: Pointer?,
        `clearUnlistedLinkedPeers`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_contact_private_payment_lists_and_process_outbound(
        `ptr`: Pointer?,
        `clearUnlistedLinkedPeers`: Byte,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_private_payment_lists_with_reservations_and_process_outbound(
        `ptr`: Pointer?,
        `updates`: RustBufferByValue,
        `clearUnlistedLinkedPeers`: Byte,
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
    external fun uniffi_paykit_fn_method_ffipaykitsdk_sync_public_endpoints_with_receiving_details(
        `ptr`: Pointer?,
        `receivingDetails`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_unblock_peer(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaykitsdk_upload_profile_avatar(
        `ptr`: Pointer?,
        `bytes`: RustBufferByValue,
        `contentType`: RustBufferByValue,
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
    external fun uniffi_paykit_fn_clone_ffipaymentreference(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffipaymentreference(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffipaymentreference_new(
        `text`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipaymentreference_export_text(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffiprivatejsonobject(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffiprivatejsonobject(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffiprivatejsonobject_new(
        `text`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffiprivatejsonobject_export_text(
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
        `receiverNoiseSecretKey`: Pointer?,
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
        `receiverNoiseSecretKey`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionaccess_export_local_secret_key(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionaccess_export_receiver_noise_secret_key(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
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
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_approve_auth_with_companion_claim(
        `ptr`: Pointer?,
        `authUrl`: RustBufferByValue,
        `expectedCapabilities`: RustBufferByValue,
        `localSecretKey`: Pointer?,
        `claim`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_import_session(
        `ptr`: Pointer?,
        `sessionSecret`: RustBufferByValue,
        `localSecretKey`: RustBufferByValue,
        `receiverNoiseSecretKey`: Pointer?,
        `requiredCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_resume_auth(
        `ptr`: Pointer?,
        `authorizationUrl`: RustBufferByValue,
        `expectedCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_in(
        `ptr`: Pointer?,
        `localSecretKey`: Pointer?,
        `receiverNoiseSecretKey`: Pointer?,
        `requiredCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_up(
        `ptr`: Pointer?,
        `localSecretKey`: Pointer?,
        `receiverNoiseSecretKey`: Pointer?,
        `homeserverPublicKey`: RustBufferByValue,
        `signupCode`: RustBufferByValue,
        `requiredCapabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_in_auth(
        `ptr`: Pointer?,
        `capabilities`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_up_auth(
        `ptr`: Pointer?,
        `capabilities`: RustBufferByValue,
        `homeserverPublicKey`: RustBufferByValue,
        `signupToken`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_clone_ffireceivernoisesecretkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_free_ffireceivernoisesecretkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffireceivernoisesecretkey_new(
        `bytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_constructor_ffireceivernoisesecretkey_random(
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffireceivernoisesecretkey_export_bytes(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
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
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_current_public_receiving_details(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_current_private_receiving_details(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_reserve_private_receiving_details(
        `ptr`: Pointer?,
        `counterparty`: RustBufferByValue,
        `counterpartyReceiverPath`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_cancel_private_receiving_detail_reservation(
        `ptr`: Pointer?,
        `cancellation`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_select_public_payment_endpoint_ids(
        `ptr`: Pointer?,
        `request`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_build_public_payment_target(
        `ptr`: Pointer?,
        `endpoint`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_select_private_payment_endpoint_ids(
        `ptr`: Pointer?,
        `request`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_method_ffisdkpaymentadapter_build_private_payment_target(
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
    external fun uniffi_paykit_fn_func_decode_sdk_state_blob_snapshot(
        `bytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_default_config(
        `receiverPath`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_default_pubky_client_config(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_encode_sdk_state_blob_snapshot(
        `snapshot`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_generate_receipt_id(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_normalize_pubky_public_key(
        `value`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
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
    external fun uniffi_paykit_fn_func_pubky_secret_key_from_bip39_mnemonic(
        `mnemonicPhrase`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_func_pubky_secret_key_from_bip39_seed(
        `seed`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    @JvmStatic
    external fun uniffi_paykit_fn_func_raw_pubky_public_key(
        `value`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_redacted_pubky_public_key(
        `value`: RustBufferByValue,
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
 * Immutable private Allowance Terms with redacted debug output.
 *
 * Applications must treat the object and every value returned by its getters
 * as sensitive. Do not include them in ordinary platform logs or diagnostics.
 */
public open class AllowanceTerms: Disposable, AllowanceTermsInterface {

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
     * Validate and create immutable Allowance Terms.
     */
    public constructor(`asset`: kotlin.String, `perPaymentAmount`: AllowanceAmountRange?, `periodLimits`: List<AllowancePeriodLimit>, `lifetimeAmountLimit`: kotlin.String?, `activeFrom`: kotlin.String?, `expiresAt`: kotlin.String?, `allowedPaymentEndpointIdentifiers`: List<kotlin.String>?) : this(
        uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffiallowanceterms_new(
                FfiConverterString.lower(`asset`),
                FfiConverterOptionalTypeFfiAllowanceAmountRange.lower(`perPaymentAmount`),
                FfiConverterSequenceTypeAllowancePeriodLimit.lower(`periodLimits`),
                FfiConverterOptionalString.lower(`lifetimeAmountLimit`),
                FfiConverterOptionalString.lower(`activeFrom`),
                FfiConverterOptionalString.lower(`expiresAt`),
                FfiConverterOptionalSequenceString.lower(`allowedPaymentEndpointIdentifiers`),
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
                    UniffiLib.uniffi_paykit_fn_free_ffiallowanceterms(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffiallowanceterms(pointer!!, status)
        }!!
    }


    /**
     * Return the optional inclusive first eligible instant.
     */
    public override fun `activeFrom`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_active_from(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return the optional exact Payment Endpoint Identifier allowlist.
     */
    public override fun `allowedPaymentEndpointIdentifiers`(): List<kotlin.String>? {
        return FfiConverterOptionalSequenceString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_allowed_payment_endpoint_identifiers(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return the exact, case-sensitive asset.
     */
    public override fun `asset`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_asset(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return the optional exclusive first ineligible instant.
     */
    public override fun `expiresAt`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_expires_at(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return the optional lifetime amount ceiling decimal spelling.
     */
    public override fun `lifetimeAmountLimit`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_lifetime_amount_limit(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return the optional inclusive per-payment amount range.
     */
    public override fun `perPaymentAmount`(): AllowanceAmountRange? {
        return FfiConverterOptionalTypeFfiAllowanceAmountRange.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_per_payment_amount(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return every independently applicable period limit.
     */
    public override fun `periodLimits`(): List<AllowancePeriodLimit> {
        return FfiConverterSequenceTypeAllowancePeriodLimit.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiallowanceterms_period_limits(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeAllowanceTerms: FfiConverter<AllowanceTerms, Pointer> {

    override fun lower(value: AllowanceTerms): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): AllowanceTerms {
        return AllowanceTerms(value)
    }

    override fun read(buf: ByteBuffer): AllowanceTerms {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: AllowanceTerms): ULong = 8UL

    override fun write(value: AllowanceTerms, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Stateful Paykit SDK runtime handle.
 */
public open class PaykitSdk: Disposable, PaykitSdkInterface {

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
    public constructor(`stateStore`: SdkStateBlobStore, `sessionProvider`: SdkPubkySessionProvider, `config`: PaykitSdkConfig) : this(
        uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_new(
                FfiConverterTypeSdkStateBlobStore.lower(`stateStore`),
                FfiConverterTypeSdkPubkySessionProvider.lower(`sessionProvider`),
                FfiConverterTypePaykitSdkConfig.lower(`config`),
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
     * Queue acceptance for a received Allowance proposal.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `acceptAllowance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `allowanceId`: kotlin.String): AllowanceRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_accept_allowance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`allowanceId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Start an Encrypted Link Handshake as the responder.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `acceptLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_accept_link_with_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue acceptance for a received Payment Request and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `acceptPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String): PaymentRequestRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_accept_payment_request(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`paymentRequestId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return received Payment Requests that need a local payer response.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `actionableReceivedPaymentRequests`(): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_actionable_received_payment_requests(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return accepted recurring Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `activeRecurringPaymentRequests`(): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_active_recurring_payment_requests(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Advance the stored Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `advanceLinkHandshake`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_advance_link_handshake(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Block a counterparty for local Paykit private workflows.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `blockPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_block_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue cancellation for a known non-terminal Payment Request.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `cancelPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): PaymentRequestRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_cancel_payment_request(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`paymentRequestId`),
                    FfiConverterOptionalString.lower(`reason`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue an empty Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `clearPrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): QueuedPrivateMessage {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_clear_private_payment_list(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeQueuedPrivateMessage.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue an empty Private Payment List and process that counterparty's queue.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `clearPrivatePaymentListAndProcessOutbound`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivatePaymentListDeliveryReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_clear_private_payment_list_and_process_outbound(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivatePaymentListDeliveryReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return this runtime's configuration.
     */
    public override fun `config`(): PaykitSdkConfig {
        return FfiConverterTypePaykitSdkConfig.lift(callWithPointer {
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
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `contactRecord`(`publicKey`: kotlin.String): ContactRecord? {
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return all local Contact Records.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `contactRecords`(): List<ContactRecord> {
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
            { FfiConverterSequenceTypeContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return the latest valid Private Payment List view for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `currentPrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivatePaymentListView? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_current_private_payment_list(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPrivatePaymentListView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resolve this identity's public profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `currentProfile`(`allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_current_profile(
                    thisPtr,
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Delete a blob by `pubky://` URI or configured Paykit profile path.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Delete this identity's Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `deletePaykitProfile`() {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_delete_paykit_profile(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return tracked Encrypted Link recovery marker state for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `encryptedLinkRecoveryMarkerStatus`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_encrypted_link_recovery_marker_status(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue a proposal withdrawal or unilateral End for accepted authority.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `endAllowance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `allowanceId`: kotlin.String): AllowanceRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_end_allowance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`allowanceId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue the current complete Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `enqueuePrivatePaymentList`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): QueuedPrivateMessage {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeQueuedPrivateMessage.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue an explicit complete Private Payment List for one counterparty receiver.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `enqueuePrivatePaymentListWithReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receivingDetails`: List<PrivateReceivingDetail>): QueuedPrivateMessage {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_enqueue_private_payment_list_with_receiving_details(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterSequenceTypePrivateReceivingDetail.lower(`receivingDetails`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeQueuedPrivateMessage.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Start or advance an Encrypted Link Handshake for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `ensureLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `maxAdvanceSteps`: kotlin.UInt): LinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_ensure_link_with_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterUInt.lower(`maxAdvanceSteps`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Export SDK-managed backup state as an opaque blob.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `exportBackupState`(): SdkBackupBlob {
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
            { FfiConverterTypeSdkBackupBlob.lift(it!!) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Export SDK-managed backup state as a hex string.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `exportBackupString`(): kotlin.String {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_export_backup_string(
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPaykitProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): PaykitProfileRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_fetch_paykit_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPaykitProfileRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch public Pubky file bytes.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch public Pubky app follows.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Pubky app profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `fetchPubkyProfile`(`publicKey`: kotlin.String): PubkyProfileRecord? {
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch a public Pubky UTF-8 text file.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return one Allowance from one exact authenticated Encrypted Link.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `getAllowance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `allowanceId`: kotlin.String): AllowanceRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_get_allowance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`allowanceId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return current identity status, when initialized.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `identityStatus`(): IdentityStatus? {
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Initialize durable SDK identity state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `initialize`(): InitializationReport {
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
            { FfiConverterTypeInitializationReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Start an Encrypted Link Handshake as the initiator.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `initiateLinkWithPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerHandshakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_initiate_link_with_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerHandshakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Prepare, store, and queue Receipt Access for private delivery.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `issueReceipt`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `draft`: ReceiptDraft): ReceiptIssuanceView {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_issue_receipt(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterTypeReceiptDraft.lower(`draft`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List issued receipts across non-blocked counterparties, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `issuedReceipts`(): List<ReceiptIssuanceView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_issued_receipts(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List issued receipts for one counterparty, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `issuedReceiptsTo`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptIssuanceView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_issued_receipts_to(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List locally tracked Linked Peer records.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `linkedPeers`(): List<LinkedPeerRecord> {
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
            { FfiConverterSequenceTypeLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return Allowances matching a local SDK filter, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `listAllowances`(`filter`: AllowanceFilter): List<AllowanceRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_list_allowances(
                    thisPtr,
                    FfiConverterTypeAllowanceFilter.lower(`filter`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return Payment Requests matching a local SDK filter.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `listPaymentRequests`(`filter`: PaymentRequestFilter): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_list_payment_requests(
                    thisPtr,
                    FfiConverterTypePaymentRequestFilter.lower(`filter`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Observe a counterparty's public recovery marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `observeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_observe_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch one public Paykit receiver marker, if present.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `paykitReceiverMarker`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): PaykitReceiverMarker? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_paykit_receiver_marker(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiPaykitReceiverMarker.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List public Paykit receiver paths for a Pubky identity.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `paykitReceiverPaths`(`publicKey`: kotlin.String): List<kotlin.String> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_paykit_receiver_paths(
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return all Payment Requests across non-blocked counterparties.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `paymentRequests`(): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_payment_requests(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return Payment Requests involving one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `paymentRequestsWith`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_payment_requests_with(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List counterparties with queued private messages ready for retry.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `pendingOutboundPrivateCounterparties`(): List<CounterpartyReceiver> {
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
            { FfiConverterSequenceTypeCounterpartyReceiver.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Prepare private contact state, then resolve private endpoints.
     *
     * Pass the last consumed list version to require a newer Private Payment
     * List after private messages have been refreshed.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `prepareAndResolvePrivateContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?, `afterPrivatePaymentListVersion`: kotlin.ULong?, `maxAdvanceSteps`: kotlin.UInt): PreparedPrivateContactPayment {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_prepare_and_resolve_private_contact_payment(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterOptionalTypeFfiPaymentAmountContext.lower(`amount`),
                    FfiConverterOptionalULong.lower(`afterPrivatePaymentListVersion`),
                    FfiConverterUInt.lower(`maxAdvanceSteps`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePreparedPrivateContactPayment.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Prepare a receipt issuance and persist it before network side effects.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `prepareReceiptIssuance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `draft`: ReceiptDraft): ReceiptIssuanceView {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_prepare_receipt_issuance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterTypeReceiptDraft.lower(`draft`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Send queued outbound private messages for one counterparty in order.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `processOutboundPrivateMessages`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): OutboundPrivateSendReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_process_outbound_private_messages(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeOutboundPrivateSendReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Process queued outbound private messages for every pending counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `processPendingPrivateMessages`(): List<OutboundPrivateCounterpartySendReport> {
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
            { FfiConverterSequenceTypeOutboundPrivateCounterpartySendReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Continue storage and Receipt Access queueing for a prepared issuance.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `processReceiptIssuance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receiptId`: kotlin.String): ReceiptIssuanceView {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_process_receipt_issuance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`receiptId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue a new Allowance proposal and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `proposeAllowance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `localRole`: AllowanceLocalRole, `terms`: AllowanceTerms): AllowanceRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_propose_allowance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterTypeAllowanceLocalRole.lower(`localRole`),
                    FfiConverterTypeAllowanceTerms.lower(`terms`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue a new Payment Request proposal and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `proposePaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `terms`: PaymentRequestTerms): PaymentRequestRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_propose_payment_request(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterTypePaymentRequestTerms.lower(`terms`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish a minimal local recovery marker for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish a blob under this identity's Paykit profile namespace.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPaykitBlob`(`blobName`: kotlin.String, `bytes`: kotlin.ByteArray): PaykitBlobRecord {
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
            { FfiConverterTypePaykitBlobRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish this identity's Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPaykitProfile`(`profile`: PaykitProfile): PaykitProfileRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_profile(
                    thisPtr,
                    FfiConverterTypePaykitProfile.lower(`profile`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaykitProfileRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish the configured local receiver marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPaykitReceiverMarker`(`capabilities`: PaykitReceiverCapabilities): PaykitReceiverMarker {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_paykit_receiver_marker(
                    thisPtr,
                    FfiConverterTypePaykitReceiverCapabilities.lower(`capabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaykitReceiverMarker.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish a public Contact Marker for a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `publishPublicContact`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_publish_public_contact(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List Receipt Access across non-blocked counterparties, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptAccess`(): List<ReceiptAccessView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipt_access(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptAccessView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List Receipt Access received from one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptAccessFrom`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptAccessView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipt_access_from(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptAccessView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List indexed Receipt Access records for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptAccessRecords`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptAccessView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipt_access_records(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptAccessView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List local receipt issuance records for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptIssuanceRecords`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<ReceiptIssuanceView> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipt_issuance_records(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptIssuanceView.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List decrypted Receipt records for one issuer, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptRecords`(`issuer`: kotlin.String, `issuerReceiverPath`: kotlin.String): List<ReceiptRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipt_records(
                    thisPtr,
                    FfiConverterString.lower(`issuer`),
                    FfiConverterString.lower(`issuerReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List decrypted receipts across non-blocked issuers, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receipts`(): List<ReceiptRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipts(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * List decrypted receipts from one issuer, newest first.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receiptsFrom`(`issuer`: kotlin.String, `issuerReceiverPath`: kotlin.String): List<ReceiptRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receipts_from(
                    thisPtr,
                    FfiConverterString.lower(`issuer`),
                    FfiConverterString.lower(`issuerReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypeReceiptRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Receive and durably persist available private messages.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receivePrivateMessages`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivateStreamIntakeReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_receive_private_messages(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivateStreamIntakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Receive private messages from every locally linked counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receivePrivateMessagesFromLinkedPeers`(): List<PrivateStreamCounterpartyIntakeReport> {
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
            { FfiConverterSequenceTypePrivateStreamCounterpartyIntakeReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return inbound Payment Requests received from one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `receivedPaymentRequestsFrom`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PaymentRequestRecord> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_received_payment_requests_from(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterSequenceTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Refresh the cached Paykit Profile for a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `refreshContactPaykitProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_refresh_contact_paykit_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue rejection for a received Allowance proposal.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `rejectAllowance`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `allowanceId`: kotlin.String): AllowanceRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_reject_allowance(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`allowanceId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeAllowanceRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue rejection for a received Payment Request and return local derived state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `rejectPaymentRequest`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `reason`: kotlin.String?): PaymentRequestRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_reject_payment_request(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`paymentRequestId`),
                    FfiConverterOptionalString.lower(`reason`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Remove a local Contact Record when it has no public marker to clean up.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removeContact`(`publicKey`: kotlin.String): ContactRecord? {
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Remove the local public recovery marker for a counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removeEncryptedLinkRecoveryMarker`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): EncryptedLinkRecoveryMarkerReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_encrypted_link_recovery_marker(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeEncryptedLinkRecoveryMarkerReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Remove the configured local receiver marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removePaykitReceiverMarker`() {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_paykit_receiver_marker(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Remove a public Contact Marker.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `removePublicContact`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String): ContactRecord? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_remove_public_contact(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterOptionalTypeFfiContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resolve display metadata for a contact.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolveContactProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_contact_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resolve payable private endpoints for one counterparty.
     *
     * Pass the last consumed list version to require a newer Private Payment
     * List. The returned version and endpoints come from the same local list
     * snapshot.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolvePrivateContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?, `afterPrivatePaymentListVersion`: kotlin.ULong?): PrivateContactPaymentResolution {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_private_contact_payment(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterOptionalTypeFfiPaymentAmountContext.lower(`amount`),
                    FfiConverterOptionalULong.lower(`afterPrivatePaymentListVersion`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivateContactPaymentResolution.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resolve public profile metadata, preferring Paykit Profile.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolveProfile`(`publicKey`: kotlin.String, `receiverPath`: kotlin.String, `allowPubkyProfileFallback`: kotlin.Boolean): ContactProfileResolution? {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_profile(
                    thisPtr,
                    FfiConverterString.lower(`publicKey`),
                    FfiConverterString.lower(`receiverPath`),
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resolve payable public Payment Endpoints for one counterparty.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resolvePublicContactPayment`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `amount`: PaymentAmountContext?): PublicContactPaymentResolution {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_resolve_public_contact_payment(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterOptionalTypeFfiPaymentAmountContext.lower(`amount`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePublicContactPaymentResolution.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Restore SDK-managed backup state from an opaque blob.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `restoreBackupState`(`backup`: SdkBackupBlob): RestoreReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_state(
                    thisPtr,
                    FfiConverterTypeSdkBackupBlob.lower(`backup`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeRestoreReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Restore SDK-managed backup state from a hex string.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `restoreBackupString`(`backup`: kotlin.String): RestoreReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_restore_backup_string(
                    thisPtr,
                    FfiConverterString.lower(`backup`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeRestoreReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `retrieveReceipt`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `receiptId`: kotlin.String): ReceiptRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_retrieve_receipt(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`receiptId`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeReceiptRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Save or update a local Contact Record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `saveContact`(`update`: ContactUpdate): ContactRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_save_contact(
                    thisPtr,
                    FfiConverterTypeContactUpdate.lower(`update`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Clear live Pubky session access and SDK-managed identity-scoped state.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signOut`(): IdentityStatus {
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
            { FfiConverterTypeIdentityStatus.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Return the current platform SDK state revision, when a state blob exists.
     */
    @Throws(PaykitException::class)
    public override fun `stateRevision`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_state_revision(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Queue a Payment Proof for an accepted Payment Request.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `submitPaymentProof`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String, `paymentRequestId`: kotlin.String, `proof`: PaymentProofSubmission): PaymentRequestRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_submit_payment_proof(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    FfiConverterString.lower(`paymentRequestId`),
                    FfiConverterTypePaymentProofSubmission.lower(`proof`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaymentRequestRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue Private Payment List updates for saved local contacts.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncContactPrivatePaymentLists`(`clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListSyncReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_contact_private_payment_lists(
                    thisPtr,
                    FfiConverterBoolean.lower(`clearUnlistedLinkedPeers`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivatePaymentListSyncReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue contact Private Payment Lists and process pending private messages.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncContactPrivatePaymentListsAndProcessOutbound`(`clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListDeliveryReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_contact_private_payment_lists_and_process_outbound(
                    thisPtr,
                    FfiConverterBoolean.lower(`clearUnlistedLinkedPeers`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivatePaymentListDeliveryReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Queue reservation-backed Private Payment Lists and process their queues.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPrivatePaymentListsWithReservationsAndProcessOutbound`(`updates`: List<PrivatePaymentListReservationUpdateInput>, `clearUnlistedLinkedPeers`: kotlin.Boolean): PrivatePaymentListDeliveryReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_private_payment_lists_with_reservations_and_process_outbound(
                    thisPtr,
                    FfiConverterSequenceTypePrivatePaymentListReservationUpdateInput.lower(`updates`),
                    FfiConverterBoolean.lower(`clearUnlistedLinkedPeers`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePrivatePaymentListDeliveryReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Retry pending public Contact Marker publication/removal work.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPublicContactMarkers`(): List<ContactRecord> {
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
            { FfiConverterSequenceTypeContactRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish current public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPublicEndpoints`(): EndpointSyncReport {
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
            { FfiConverterTypeEndpointSyncReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Publish explicit public receiving details and remove stale SDK-managed endpoints.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `syncPublicEndpointsWithReceivingDetails`(`receivingDetails`: List<PublicReceivingDetail>): EndpointSyncReport {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_sync_public_endpoints_with_receiving_details(
                    thisPtr,
                    FfiConverterSequenceTypePublicReceivingDetail.lower(`receivingDetails`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeEndpointSyncReport.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Remove a local peer block and return the peer to NotLinked.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `unblockPeer`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): LinkedPeerRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_unblock_peer(
                    thisPtr,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeLinkedPeerRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Upload profile avatar bytes and return the published blob record.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `uploadProfileAvatar`(`bytes`: kotlin.ByteArray, `contentType`: kotlin.String): PaykitBlobRecord {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipaykitsdk_upload_profile_avatar(
                    thisPtr,
                    FfiConverterByteArray.lower(`bytes`),
                    FfiConverterString.lower(`contentType`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePaykitBlobRecord.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }






    public companion object {

        /**
         * Create an SDK runtime with payment adapter callbacks.
         */
        @Throws(PaykitException::class)
        public fun `withPaymentAdapter`(`stateStore`: SdkStateBlobStore, `sessionProvider`: SdkPubkySessionProvider, `paymentAdapter`: SdkPaymentAdapter, `config`: PaykitSdkConfig): PaykitSdk {
            return FfiConverterTypePaykitSdk.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter(
                    FfiConverterTypeSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypeSdkPaymentAdapter.lower(`paymentAdapter`),
                    FfiConverterTypePaykitSdkConfig.lower(`config`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


        /**
         * Create an SDK runtime with payment adapter callbacks and Pubky client configuration.
         */
        @Throws(PaykitException::class)
        public fun `withPaymentAdapterAndPubkyClientConfig`(`stateStore`: SdkStateBlobStore, `sessionProvider`: SdkPubkySessionProvider, `paymentAdapter`: SdkPaymentAdapter, `config`: PaykitSdkConfig, `pubkyClient`: PubkyClientConfig): PaykitSdk {
            return FfiConverterTypePaykitSdk.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_payment_adapter_and_pubky_client_config(
                    FfiConverterTypeSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypeSdkPaymentAdapter.lower(`paymentAdapter`),
                    FfiConverterTypePaykitSdkConfig.lower(`config`),
                    FfiConverterTypePubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


        /**
         * Create an SDK runtime with explicit Pubky client configuration.
         */
        @Throws(PaykitException::class)
        public fun `withPubkyClientConfig`(`stateStore`: SdkStateBlobStore, `sessionProvider`: SdkPubkySessionProvider, `config`: PaykitSdkConfig, `pubkyClient`: PubkyClientConfig): PaykitSdk {
            return FfiConverterTypePaykitSdk.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipaykitsdk_with_pubky_client_config(
                    FfiConverterTypeSdkStateBlobStore.lower(`stateStore`),
                    FfiConverterTypeSdkPubkySessionProvider.lower(`sessionProvider`),
                    FfiConverterTypePaykitSdkConfig.lower(`config`),
                    FfiConverterTypePubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


    }

}





public object FfiConverterTypePaykitSdk: FfiConverter<PaykitSdk, Pointer> {

    override fun lower(value: PaykitSdk): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PaykitSdk {
        return PaykitSdk(value)
    }

    override fun read(buf: ByteBuffer): PaykitSdk {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PaykitSdk): ULong = 8UL

    override fun write(value: PaykitSdk, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Payment adapter payload text with redacted debug output.
 */
public open class PaymentPayload: Disposable, PaymentPayloadInterface {

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





public object FfiConverterTypePaymentPayload: FfiConverter<PaymentPayload, Pointer> {

    override fun lower(value: PaymentPayload): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PaymentPayload {
        return PaymentPayload(value)
    }

    override fun read(buf: ByteBuffer): PaymentPayload {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PaymentPayload): ULong = 8UL

    override fun write(value: PaymentPayload, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Payment Reference text with redacted debug output.
 */
public open class PaymentReference: Disposable, PaymentReferenceInterface {

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
     * Create a Payment Reference after validating it.
     */
    public constructor(`text`: kotlin.String) : this(
        uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipaymentreference_new(
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
                    UniffiLib.uniffi_paykit_fn_free_ffipaymentreference(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffipaymentreference(pointer!!, status)
        }!!
    }


    /**
     * Export the reference text for explicit payment execution or display.
     */
    public override fun `exportText`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipaymentreference_export_text(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypePaymentReference: FfiConverter<PaymentReference, Pointer> {

    override fun lower(value: PaymentReference): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PaymentReference {
        return PaymentReference(value)
    }

    override fun read(buf: ByteBuffer): PaymentReference {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PaymentReference): ULong = 8UL

    override fun write(value: PaymentReference, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Private JSON object with redacted debug output.
 */
public open class PrivateJsonObject: Disposable, PrivateJsonObjectInterface {

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
     * Create a private JSON object after validating it.
     */
    public constructor(`text`: kotlin.String) : this(
        uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffiprivatejsonobject_new(
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
                    UniffiLib.uniffi_paykit_fn_free_ffiprivatejsonobject(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffiprivatejsonobject(pointer!!, status)
        }!!
    }


    /**
     * Export the JSON text for explicit app display, storage, or payment execution.
     */
    public override fun `exportText`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffiprivatejsonobject_export_text(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypePrivateJsonObject: FfiConverter<PrivateJsonObject, Pointer> {

    override fun lower(value: PrivateJsonObject): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PrivateJsonObject {
        return PrivateJsonObject(value)
    }

    override fun read(buf: ByteBuffer): PrivateJsonObject {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PrivateJsonObject): ULong = 8UL

    override fun write(value: PrivateJsonObject, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Private workflow error with redacted default context.
 */
public open class PrivateOperationError: Disposable, PrivateOperationErrorInterface {

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





public object FfiConverterTypePrivateOperationError: FfiConverter<PrivateOperationError, Pointer> {

    override fun lower(value: PrivateOperationError): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PrivateOperationError {
        return PrivateOperationError(value)
    }

    override fun read(buf: ByteBuffer): PrivateOperationError {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PrivateOperationError): ULong = 8UL

    override fun write(value: PrivateOperationError, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Pending Pubky auth request.
 */
public open class PubkyAuthRequest: Disposable, PubkyAuthRequestInterface {

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
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
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
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Wait for auth approval using the receiver's persisted Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `complete`(`localSecretKey`: PubkyLocalSecretKey?, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkyauthrequest_complete(
                    thisPtr,
                    FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterTypeReceiverNoiseSecretKey.lower(`receiverNoiseSecretKey`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }







    public companion object

}





public object FfiConverterTypePubkyAuthRequest: FfiConverter<PubkyAuthRequest, Pointer> {

    override fun lower(value: PubkyAuthRequest): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PubkyAuthRequest {
        return PubkyAuthRequest(value)
    }

    override fun read(buf: ByteBuffer): PubkyAuthRequest {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PubkyAuthRequest): ULong = 8UL

    override fun write(value: PubkyAuthRequest, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Local Pubky secret key bytes supplied by platform secure storage.
 */
public open class PubkyLocalSecretKey: Disposable, PubkyLocalSecretKeyInterface {

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





public object FfiConverterTypePubkyLocalSecretKey: FfiConverter<PubkyLocalSecretKey, Pointer> {

    override fun lower(value: PubkyLocalSecretKey): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PubkyLocalSecretKey {
        return PubkyLocalSecretKey(value)
    }

    override fun read(buf: ByteBuffer): PubkyLocalSecretKey {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PubkyLocalSecretKey): ULong = 8UL

    override fun write(value: PubkyLocalSecretKey, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Live Pubky access material supplied by platform session storage.
 */
public open class PubkySessionAccess: Disposable, PubkySessionAccessInterface {

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
    public constructor(`sessionSecret`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey?, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffipubkysessionaccess_new(
                FfiConverterString.lower(`sessionSecret`),
                FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                FfiConverterTypeReceiverNoiseSecretKey.lower(`receiverNoiseSecretKey`),
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
    public override fun `exportLocalSecretKey`(): PubkyLocalSecretKey? {
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
     * Export the receiver Noise secret key for platform secure storage.
     */
    public override fun `exportReceiverNoiseSecretKey`(): ReceiverNoiseSecretKey {
        return FfiConverterTypeReceiverNoiseSecretKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionaccess_export_receiver_noise_secret_key(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
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





public object FfiConverterTypePubkySessionAccess: FfiConverter<PubkySessionAccess, Pointer> {

    override fun lower(value: PubkySessionAccess): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PubkySessionAccess {
        return PubkySessionAccess(value)
    }

    override fun read(buf: ByteBuffer): PubkySessionAccess {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PubkySessionAccess): ULong = 8UL

    override fun write(value: PubkySessionAccess, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Pubky session bootstrap helper.
 */
public open class PubkySessionBootstrap: Disposable, PubkySessionBootstrapInterface {

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
        uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
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
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `approveAuth`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey) {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_approve_auth(
                    thisPtr,
                    FfiConverterString.lower(`authUrl`),
                    FfiConverterString.lower(`expectedCapabilities`),
                    FfiConverterTypePubkyLocalSecretKey.lower(`localSecretKey`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Deliver a signed application-defined claim, then approve Pubky Auth.
     *
     * This high-level operation owns validation, request-bound signing,
     * channel derivation, encryption, relay delivery, and approval ordering.
     */
    @Throws(PubkyAuthCompanionClaimApprovalException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `approveAuthWithCompanionClaim`(`authUrl`: kotlin.String, `expectedCapabilities`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey, `claim`: PubkyAuthCompanionClaim) {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_approve_auth_with_companion_claim(
                    thisPtr,
                    FfiConverterString.lower(`authUrl`),
                    FfiConverterString.lower(`expectedCapabilities`),
                    FfiConverterTypePubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterTypePubkyAuthCompanionClaim.lower(`claim`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
            // lift function
            { Unit },

            // Error FFI converter
            PubkyAuthCompanionClaimApprovalExceptionErrorHandler,
        )
    }

    /**
     * Import an exported Pubky session secret and its persisted receiver Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `importSession`(`sessionSecret`: kotlin.String, `localSecretKey`: PubkyLocalSecretKey?, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_import_session(
                    thisPtr,
                    FfiConverterString.lower(`sessionSecret`),
                    FfiConverterOptionalTypeFfiPubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterTypeReceiverNoiseSecretKey.lower(`receiverNoiseSecretKey`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Resume a short-lived auth flow from its authorization URL.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `resumeAuth`(`authorizationUrl`: kotlin.String, `expectedCapabilities`: kotlin.String): PubkyAuthRequest {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_resume_auth(
                    thisPtr,
                    FfiConverterString.lower(`authorizationUrl`),
                    FfiConverterString.lower(`expectedCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_pointer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_pointer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_pointer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_pointer(future) },
            // lift function
            { FfiConverterTypePubkyAuthRequest.lift(it!!) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Sign in with the receiver's persisted Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signIn`(`localSecretKey`: PubkyLocalSecretKey, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_in(
                    thisPtr,
                    FfiConverterTypePubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterTypeReceiverNoiseSecretKey.lower(`receiverNoiseSecretKey`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Sign up on a homeserver with the receiver-owned Noise key.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `signUp`(`localSecretKey`: PubkyLocalSecretKey, `receiverNoiseSecretKey`: ReceiverNoiseSecretKey, `homeserverPublicKey`: kotlin.String, `signupCode`: kotlin.String?, `requiredCapabilities`: kotlin.String): PubkySessionBootstrapResult {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_sign_up(
                    thisPtr,
                    FfiConverterTypePubkyLocalSecretKey.lower(`localSecretKey`),
                    FfiConverterTypeReceiverNoiseSecretKey.lower(`receiverNoiseSecretKey`),
                    FfiConverterString.lower(`homeserverPublicKey`),
                    FfiConverterOptionalString.lower(`signupCode`),
                    FfiConverterString.lower(`requiredCapabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypePubkySessionBootstrapResult.lift(it) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Start a sign-in auth flow for an external signer.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `startSignInAuth`(`capabilities`: kotlin.String): PubkyAuthRequest {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_in_auth(
                    thisPtr,
                    FfiConverterString.lower(`capabilities`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_pointer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_pointer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_pointer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_pointer(future) },
            // lift function
            { FfiConverterTypePubkyAuthRequest.lift(it!!) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }

    /**
     * Start a signup auth flow for an external signer.
     */
    @Throws(PaykitException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public override suspend fun `startSignUpAuth`(`capabilities`: kotlin.String, `homeserverPublicKey`: kotlin.String, `signupToken`: kotlin.String?): PubkyAuthRequest {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.uniffi_paykit_fn_method_ffipubkysessionbootstrap_start_sign_up_auth(
                    thisPtr,
                    FfiConverterString.lower(`capabilities`),
                    FfiConverterString.lower(`homeserverPublicKey`),
                    FfiConverterOptionalString.lower(`signupToken`),
                )
            },
            { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_pointer(future, callback, continuation) },
            { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_pointer(future, continuation) },
            { future -> UniffiLib.ffi_paykit_rust_future_free_pointer(future) },
            { future -> UniffiLib.ffi_paykit_rust_future_cancel_pointer(future) },
            // lift function
            { FfiConverterTypePubkyAuthRequest.lift(it!!) },
            // Error FFI converter
            PaykitExceptionErrorHandler,
        )
    }






    public companion object {

        /**
         * Create a Pubky session bootstrap helper with explicit Pubky client configuration.
         */
        @Throws(PaykitException::class)
        public fun `withPubkyClientConfig`(`pubkyClient`: PubkyClientConfig): PubkySessionBootstrap {
            return FfiConverterTypePubkySessionBootstrap.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffipubkysessionbootstrap_with_pubky_client_config(
                    FfiConverterTypePubkyClientConfig.lower(`pubkyClient`),
                    uniffiRustCallStatus,
                )
            }!!)
        }


    }

}





public object FfiConverterTypePubkySessionBootstrap: FfiConverter<PubkySessionBootstrap, Pointer> {

    override fun lower(value: PubkySessionBootstrap): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): PubkySessionBootstrap {
        return PubkySessionBootstrap(value)
    }

    override fun read(buf: ByteBuffer): PubkySessionBootstrap {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: PubkySessionBootstrap): ULong = 8UL

    override fun write(value: PubkySessionBootstrap, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Receiver-scoped Noise secret key bytes supplied by platform secure storage.
 */
public open class ReceiverNoiseSecretKey: Disposable, ReceiverNoiseSecretKeyInterface {

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
     * Create a receiver Noise secret key from platform secure storage bytes.
     */
    public constructor(`bytes`: kotlin.ByteArray) : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.uniffi_paykit_fn_constructor_ffireceivernoisesecretkey_new(
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
                    UniffiLib.uniffi_paykit_fn_free_ffireceivernoisesecretkey(ptr, status)
                }
            }
        }
    }

    public fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.uniffi_paykit_fn_clone_ffireceivernoisesecretkey(pointer!!, status)
        }!!
    }


    /**
     * Export the raw bytes for platform secure storage.
     */
    public override fun `exportBytes`(): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffireceivernoisesecretkey_export_bytes(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }






    public companion object {

        /**
         * Generate a fresh receiver Noise secret key.
         */
        public fun `random`(): ReceiverNoiseSecretKey {
            return FfiConverterTypeReceiverNoiseSecretKey.lift(uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_constructor_ffireceivernoisesecretkey_random(
                    uniffiRustCallStatus,
                )
            }!!)
        }


    }

}





public object FfiConverterTypeReceiverNoiseSecretKey: FfiConverter<ReceiverNoiseSecretKey, Pointer> {

    override fun lower(value: ReceiverNoiseSecretKey): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): ReceiverNoiseSecretKey {
        return ReceiverNoiseSecretKey(value)
    }

    override fun read(buf: ByteBuffer): ReceiverNoiseSecretKey {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: ReceiverNoiseSecretKey): ULong = 8UL

    override fun write(value: ReceiverNoiseSecretKey, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Reservation attribution metadata with redacted debug output.
 */
public open class ReservationAttribution: Disposable, ReservationAttributionInterface {

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





public object FfiConverterTypeReservationAttribution: FfiConverter<ReservationAttribution, Pointer> {

    override fun lower(value: ReservationAttribution): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): ReservationAttribution {
        return ReservationAttribution(value)
    }

    override fun read(buf: ByteBuffer): ReservationAttribution {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: ReservationAttribution): ULong = 8UL

    override fun write(value: ReservationAttribution, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * SDK backup blob owned by the app.
 */
public open class SdkBackupBlob: Disposable, SdkBackupBlobInterface {

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





public object FfiConverterTypeSdkBackupBlob: FfiConverter<SdkBackupBlob, Pointer> {

    override fun lower(value: SdkBackupBlob): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): SdkBackupBlob {
        return SdkBackupBlob(value)
    }

    override fun read(buf: ByteBuffer): SdkBackupBlob {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SdkBackupBlob): ULong = 8UL

    override fun write(value: SdkBackupBlob, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Platform-owned, mode-specific payment adapter callbacks.
 *
 * Public callbacks never receive private values, and private callbacks never
 * receive public values.
 */
public open class SdkPaymentAdapterImpl: Disposable, SdkPaymentAdapter {

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
     * Return receiving details intended for public Payment Endpoints.
     */
    @Throws(PaykitException::class)
    public override fun `currentPublicReceivingDetails`(): List<PublicReceivingDetail> {
        return FfiConverterSequenceTypePublicReceivingDetail.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_current_public_receiving_details(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return receiving details for one counterparty's Private Payment List.
     */
    @Throws(PaykitException::class)
    public override fun `currentPrivateReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): List<PrivateReceivingDetail> {
        return FfiConverterSequenceTypePrivateReceivingDetail.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_current_private_receiving_details(
                    it,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Reserve receiving details for a counterparty's Private Payment List.
     */
    @Throws(PaykitException::class)
    public override fun `reservePrivateReceivingDetails`(`counterparty`: kotlin.String, `counterpartyReceiverPath`: kotlin.String): PrivateReceivingDetailReservationResponse {
        return FfiConverterTypePrivateReceivingDetailReservationResponse.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_reserve_private_receiving_details(
                    it,
                    FfiConverterString.lower(`counterparty`),
                    FfiConverterString.lower(`counterpartyReceiverPath`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Cancel a previously reserved receiving detail.
     */
    @Throws(PaykitException::class)
    public override fun `cancelPrivateReceivingDetailReservation`(`cancellation`: PrivatePaymentEndpointReservationCancellation) {
        callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_cancel_private_receiving_detail_reservation(
                    it,
                    FfiConverterTypePrivatePaymentEndpointReservationCancellation.lower(`cancellation`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    /**
     * Return payable public candidate ids in adapter-preferred order.
     */
    @Throws(PaykitException::class)
    public override fun `selectPublicPaymentEndpointIds`(`request`: PublicPaymentEndpointSelectionRequest): List<kotlin.String> {
        return FfiConverterSequenceString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_select_public_payment_endpoint_ids(
                    it,
                    FfiConverterTypePublicPaymentEndpointSelectionRequest.lower(`request`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Build a payment target from a payable public endpoint.
     */
    @Throws(PaykitException::class)
    public override fun `buildPublicPaymentTarget`(`endpoint`: PublicPaymentEndpointCandidate): PaymentTarget {
        return FfiConverterTypePaymentTarget.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_build_public_payment_target(
                    it,
                    FfiConverterTypePublicPaymentEndpointCandidate.lower(`endpoint`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Return payable private candidate ids in adapter-preferred order.
     */
    @Throws(PaykitException::class)
    public override fun `selectPrivatePaymentEndpointIds`(`request`: PrivatePaymentEndpointSelectionRequest): List<kotlin.String> {
        return FfiConverterSequenceString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_select_private_payment_endpoint_ids(
                    it,
                    FfiConverterTypePrivatePaymentEndpointSelectionRequest.lower(`request`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    /**
     * Build a payment target from a payable private endpoint.
     */
    @Throws(PaykitException::class)
    public override fun `buildPrivatePaymentTarget`(`endpoint`: PrivatePaymentEndpointCandidate): PaymentTarget {
        return FfiConverterTypePaymentTarget.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpaymentadapter_build_private_payment_target(
                    it,
                    FfiConverterTypePrivatePaymentEndpointCandidate.lower(`endpoint`),
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeSdkPaymentAdapter: FfiConverter<SdkPaymentAdapter, Pointer> {
    internal val handleMap = UniffiHandleMap<SdkPaymentAdapter>()

    override fun lower(value: SdkPaymentAdapter): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): SdkPaymentAdapter {
        return SdkPaymentAdapterImpl(value)
    }

    override fun read(buf: ByteBuffer): SdkPaymentAdapter {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SdkPaymentAdapter): ULong = 8UL

    override fun write(value: SdkPaymentAdapter, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}


// Put the implementation in an object so we don't pollute the top-level namespace
internal object uniffiCallbackInterfaceFfiSdkPaymentAdapter {
    internal object `currentPublicReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod0 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`currentPublicReceivingDetails`(
                )
            }
            val writeReturn = { uniffiResultValue: List<PublicReceivingDetail> ->
                uniffiOutReturn.setValue(FfiConverterSequenceTypePublicReceivingDetail.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `currentPrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod1 {
        override fun callback (
            `uniffiHandle`: Long,
            `counterparty`: RustBufferByValue,
            `counterpartyReceiverPath`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`currentPrivateReceivingDetails`(
                    FfiConverterString.lift(`counterparty`),
                    FfiConverterString.lift(`counterpartyReceiverPath`),
                )
            }
            val writeReturn = { uniffiResultValue: List<PrivateReceivingDetail> ->
                uniffiOutReturn.setValue(FfiConverterSequenceTypePrivateReceivingDetail.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `reservePrivateReceivingDetails`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod2 {
        override fun callback (
            `uniffiHandle`: Long,
            `counterparty`: RustBufferByValue,
            `counterpartyReceiverPath`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`reservePrivateReceivingDetails`(
                    FfiConverterString.lift(`counterparty`),
                    FfiConverterString.lift(`counterpartyReceiverPath`),
                )
            }
            val writeReturn = { uniffiResultValue: PrivateReceivingDetailReservationResponse ->
                uniffiOutReturn.setValue(FfiConverterTypePrivateReceivingDetailReservationResponse.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `cancelPrivateReceivingDetailReservation`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod3 {
        override fun callback (
            `uniffiHandle`: Long,
            `cancellation`: RustBufferByValue,
            `uniffiOutReturn`: Pointer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`cancelPrivateReceivingDetailReservation`(
                    FfiConverterTypePrivatePaymentEndpointReservationCancellation.lift(`cancellation`),
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
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `selectPublicPaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod4 {
        override fun callback (
            `uniffiHandle`: Long,
            `request`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`selectPublicPaymentEndpointIds`(
                    FfiConverterTypePublicPaymentEndpointSelectionRequest.lift(`request`),
                )
            }
            val writeReturn = { uniffiResultValue: List<kotlin.String> ->
                uniffiOutReturn.setValue(FfiConverterSequenceString.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `buildPublicPaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod5 {
        override fun callback (
            `uniffiHandle`: Long,
            `endpoint`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`buildPublicPaymentTarget`(
                    FfiConverterTypePublicPaymentEndpointCandidate.lift(`endpoint`),
                )
            }
            val writeReturn = { uniffiResultValue: PaymentTarget ->
                uniffiOutReturn.setValue(FfiConverterTypePaymentTarget.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `selectPrivatePaymentEndpointIds`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod6 {
        override fun callback (
            `uniffiHandle`: Long,
            `request`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`selectPrivatePaymentEndpointIds`(
                    FfiConverterTypePrivatePaymentEndpointSelectionRequest.lift(`request`),
                )
            }
            val writeReturn = { uniffiResultValue: List<kotlin.String> ->
                uniffiOutReturn.setValue(FfiConverterSequenceString.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `buildPrivatePaymentTarget`: UniffiCallbackInterfaceFfiSdkPaymentAdapterMethod7 {
        override fun callback (
            `uniffiHandle`: Long,
            `endpoint`: RustBufferByValue,
            `uniffiOutReturn`: RustBuffer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPaymentAdapter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`buildPrivatePaymentTarget`(
                    FfiConverterTypePrivatePaymentEndpointCandidate.lift(`endpoint`),
                )
            }
            val writeReturn = { uniffiResultValue: PaymentTarget ->
                uniffiOutReturn.setValue(FfiConverterTypePaymentTarget.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeSdkPaymentAdapter.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceFfiSdkPaymentAdapter(
        `currentPublicReceivingDetails`,
        `currentPrivateReceivingDetails`,
        `reservePrivateReceivingDetails`,
        `cancelPrivateReceivingDetailReservation`,
        `selectPublicPaymentEndpointIds`,
        `buildPublicPaymentTarget`,
        `selectPrivatePaymentEndpointIds`,
        `buildPrivatePaymentTarget`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_paykit_fn_init_callback_vtable_ffisdkpaymentadapter(vtable)
    }
}



/**
 * Platform-owned Pubky session provider.
 */
public open class SdkPubkySessionProviderImpl: Disposable, SdkPubkySessionProvider {

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
    @Throws(PaykitException::class)
    public override fun `loadSessionAccess`(): PubkySessionAccess? {
        return FfiConverterOptionalTypeFfiPubkySessionAccess.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
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
    @Throws(PaykitException::class)
    public override fun `publicStorageAvailable`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
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
    @Throws(PaykitException::class)
    public override fun `clearSessionAccess`() {
        callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkpubkysessionprovider_clear_session_access(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }







    public companion object

}





public object FfiConverterTypeSdkPubkySessionProvider: FfiConverter<SdkPubkySessionProvider, Pointer> {
    internal val handleMap = UniffiHandleMap<SdkPubkySessionProvider>()

    override fun lower(value: SdkPubkySessionProvider): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): SdkPubkySessionProvider {
        return SdkPubkySessionProviderImpl(value)
    }

    override fun read(buf: ByteBuffer): SdkPubkySessionProvider {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SdkPubkySessionProvider): ULong = 8UL

    override fun write(value: SdkPubkySessionProvider, buf: ByteBuffer) {
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
            val uniffiObj = FfiConverterTypeSdkPubkySessionProvider.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`loadSessionAccess`(
                )
            }
            val writeReturn = { uniffiResultValue: PubkySessionAccess? ->
                uniffiOutReturn.setValue(FfiConverterOptionalTypeFfiPubkySessionAccess.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `publicStorageAvailable`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod1 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: ByteByReference,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPubkySessionProvider.handleMap.get(uniffiHandle)
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
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object `clearSessionAccess`: UniffiCallbackInterfaceFfiSdkPubkySessionProviderMethod2 {
        override fun callback (
            `uniffiHandle`: Long,
            `uniffiOutReturn`: Pointer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeSdkPubkySessionProvider.handleMap.get(uniffiHandle)
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
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeSdkPubkySessionProvider.handleMap.remove(handle)
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
public open class SdkStateBlob: Disposable, SdkStateBlobInterface {

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





public object FfiConverterTypeSdkStateBlob: FfiConverter<SdkStateBlob, Pointer> {

    override fun lower(value: SdkStateBlob): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): SdkStateBlob {
        return SdkStateBlob(value)
    }

    override fun read(buf: ByteBuffer): SdkStateBlob {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SdkStateBlob): ULong = 8UL

    override fun write(value: SdkStateBlob, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



/**
 * Platform-owned durable blob store for SDK state.
 */
public open class SdkStateBlobStoreImpl: Disposable, SdkStateBlobStore {

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
    @Throws(PaykitException::class)
    public override fun `loadStateBlob`(): SdkStateBlobSnapshot? {
        return FfiConverterOptionalTypeFfiSdkStateBlobSnapshot.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
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
    @Throws(PaykitException::class)
    public override fun `saveStateBlobAtomically`(`blob`: SdkStateBlob, `expectedRevision`: kotlin.String?): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.uniffi_paykit_fn_method_ffisdkstateblobstore_save_state_blob_atomically(
                    it,
                    FfiConverterTypeSdkStateBlob.lower(`blob`),
                    FfiConverterOptionalString.lower(`expectedRevision`),
                    uniffiRustCallStatus,
                )
            }
        })
    }







    public companion object

}





public object FfiConverterTypeSdkStateBlobStore: FfiConverter<SdkStateBlobStore, Pointer> {
    internal val handleMap = UniffiHandleMap<SdkStateBlobStore>()

    override fun lower(value: SdkStateBlobStore): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): SdkStateBlobStore {
        return SdkStateBlobStoreImpl(value)
    }

    override fun read(buf: ByteBuffer): SdkStateBlobStore {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SdkStateBlobStore): ULong = 8UL

    override fun write(value: SdkStateBlobStore, buf: ByteBuffer) {
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
            val uniffiObj = FfiConverterTypeSdkStateBlobStore.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`loadStateBlob`(
                )
            }
            val writeReturn = { uniffiResultValue: SdkStateBlobSnapshot? ->
                uniffiOutReturn.setValue(FfiConverterOptionalTypeFfiSdkStateBlobSnapshot.lower(uniffiResultValue))
            }
            uniffiTraitInterfaceCallWithError(
                uniffiCallStatus,
                makeCall,
                writeReturn,
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
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
            val uniffiObj = FfiConverterTypeSdkStateBlobStore.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`saveStateBlobAtomically`(
                    FfiConverterTypeSdkStateBlob.lift(`blob`!!),
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
            ) { e: PaykitException -> FfiConverterTypePaykitError.lower(e) }
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeSdkStateBlobStore.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceFfiSdkStateBlobStore(
        `loadStateBlob`,
        `saveStateBlobAtomically`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_paykit_fn_init_callback_vtable_ffisdkstateblobstore(vtable)
    }
}




public object FfiConverterTypeAllowanceAmountRange: FfiConverterRustBuffer<AllowanceAmountRange> {
    override fun read(buf: ByteBuffer): AllowanceAmountRange {
        return AllowanceAmountRange(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: AllowanceAmountRange): ULong = (
            FfiConverterString.allocationSize(value.`minimum`) +
            FfiConverterString.allocationSize(value.`maximum`)
    )

    override fun write(value: AllowanceAmountRange, buf: ByteBuffer) {
        FfiConverterString.write(value.`minimum`, buf)
        FfiConverterString.write(value.`maximum`, buf)
    }
}




public object FfiConverterTypeAllowanceFilter: FfiConverterRustBuffer<AllowanceFilter> {
    override fun read(buf: ByteBuffer): AllowanceFilter {
        return AllowanceFilter(
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiAllowanceLocalRole.read(buf),
            FfiConverterSequenceTypeAllowanceLifecycleState.read(buf),
        )
    }

    override fun allocationSize(value: AllowanceFilter): ULong = (
            FfiConverterOptionalString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiAllowanceLocalRole.allocationSize(value.`localRole`) +
            FfiConverterSequenceTypeAllowanceLifecycleState.allocationSize(value.`states`)
    )

    override fun write(value: AllowanceFilter, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`counterparty`, buf)
        FfiConverterOptionalString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiAllowanceLocalRole.write(value.`localRole`, buf)
        FfiConverterSequenceTypeAllowanceLifecycleState.write(value.`states`, buf)
    }
}




public object FfiConverterTypeAllowancePeriod: FfiConverterRustBuffer<AllowancePeriod> {
    override fun read(buf: ByteBuffer): AllowancePeriod {
        return AllowancePeriod(
            FfiConverterString.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: AllowancePeriod): ULong = (
            FfiConverterString.allocationSize(value.`kind`) +
            FfiConverterULong.allocationSize(value.`every`) +
            FfiConverterString.allocationSize(value.`unit`) +
            FfiConverterOptionalString.allocationSize(value.`anchor`)
    )

    override fun write(value: AllowancePeriod, buf: ByteBuffer) {
        FfiConverterString.write(value.`kind`, buf)
        FfiConverterULong.write(value.`every`, buf)
        FfiConverterString.write(value.`unit`, buf)
        FfiConverterOptionalString.write(value.`anchor`, buf)
    }
}




public object FfiConverterTypeAllowancePeriodLimit: FfiConverterRustBuffer<AllowancePeriodLimit> {
    override fun read(buf: ByteBuffer): AllowancePeriodLimit {
        return AllowancePeriodLimit(
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypeAllowancePeriod.read(buf),
        )
    }

    override fun allocationSize(value: AllowancePeriodLimit): ULong = (
            FfiConverterOptionalString.allocationSize(value.`amountLimit`) +
            FfiConverterOptionalULong.allocationSize(value.`paymentCountLimit`) +
            FfiConverterTypeAllowancePeriod.allocationSize(value.`period`)
    )

    override fun write(value: AllowancePeriodLimit, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`amountLimit`, buf)
        FfiConverterOptionalULong.write(value.`paymentCountLimit`, buf)
        FfiConverterTypeAllowancePeriod.write(value.`period`, buf)
    }
}




public object FfiConverterTypeAllowanceRecord: FfiConverterRustBuffer<AllowanceRecord> {
    override fun read(buf: ByteBuffer): AllowanceRecord {
        return AllowanceRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiAllowanceLocalRole.read(buf),
            FfiConverterTypeAllowanceLifecycleState.read(buf),
            FfiConverterTypeAllowanceHistoryStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiAllowanceTerms.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: AllowanceRecord): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`allowanceId`) +
            FfiConverterOptionalTypeFfiAllowanceLocalRole.allocationSize(value.`localRole`) +
            FfiConverterTypeAllowanceLifecycleState.allocationSize(value.`state`) +
            FfiConverterTypeAllowanceHistoryStatus.allocationSize(value.`historyStatus`) +
            FfiConverterOptionalString.allocationSize(value.`proposalEventId`) +
            FfiConverterOptionalTypeFfiAllowanceTerms.allocationSize(value.`terms`) +
            FfiConverterOptionalULong.allocationSize(value.`proposalStreamItemId`) +
            FfiConverterOptionalULong.allocationSize(value.`proposalOutboundMessageId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`proposalOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`acceptanceEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`acceptanceOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`rejectionEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`rejectionOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`endEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`endOutboundStatus`) +
            FfiConverterSequenceString.allocationSize(value.`pendingCausalEventIds`) +
            FfiConverterSequenceString.allocationSize(value.`conflictEventIds`) +
            FfiConverterOptionalULong.allocationSize(value.`lastStreamItemId`) +
            FfiConverterOptionalULong.allocationSize(value.`lastOutboundMessageId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`lastOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`lastEventAt`) +
            FfiConverterOptionalString.allocationSize(value.`invalidReason`)
    )

    override fun write(value: AllowanceRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`allowanceId`, buf)
        FfiConverterOptionalTypeFfiAllowanceLocalRole.write(value.`localRole`, buf)
        FfiConverterTypeAllowanceLifecycleState.write(value.`state`, buf)
        FfiConverterTypeAllowanceHistoryStatus.write(value.`historyStatus`, buf)
        FfiConverterOptionalString.write(value.`proposalEventId`, buf)
        FfiConverterOptionalTypeFfiAllowanceTerms.write(value.`terms`, buf)
        FfiConverterOptionalULong.write(value.`proposalStreamItemId`, buf)
        FfiConverterOptionalULong.write(value.`proposalOutboundMessageId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`proposalOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`acceptanceEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`acceptanceOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`rejectionEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`rejectionOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`endEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`endOutboundStatus`, buf)
        FfiConverterSequenceString.write(value.`pendingCausalEventIds`, buf)
        FfiConverterSequenceString.write(value.`conflictEventIds`, buf)
        FfiConverterOptionalULong.write(value.`lastStreamItemId`, buf)
        FfiConverterOptionalULong.write(value.`lastOutboundMessageId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`lastOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`lastEventAt`, buf)
        FfiConverterOptionalString.write(value.`invalidReason`, buf)
    }
}




public object FfiConverterTypeBillingPeriod: FfiConverterRustBuffer<BillingPeriod> {
    override fun read(buf: ByteBuffer): BillingPeriod {
        return BillingPeriod(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: BillingPeriod): ULong = (
            FfiConverterString.allocationSize(value.`startsAt`) +
            FfiConverterString.allocationSize(value.`endsAt`)
    )

    override fun write(value: BillingPeriod, buf: ByteBuffer) {
        FfiConverterString.write(value.`startsAt`, buf)
        FfiConverterString.write(value.`endsAt`, buf)
    }
}




public object FfiConverterTypeContactProfileResolution: FfiConverterRustBuffer<ContactProfileResolution> {
    override fun read(buf: ByteBuffer): ContactProfileResolution {
        return ContactProfileResolution(
            FfiConverterString.read(buf),
            FfiConverterTypeContactProfileSource.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaykitProfile.read(buf),
            FfiConverterOptionalTypeFfiPubkyProfile.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: ContactProfileResolution): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypeContactProfileSource.allocationSize(value.`source`) +
            FfiConverterOptionalString.allocationSize(value.`displayName`) +
            FfiConverterOptionalString.allocationSize(value.`imageUri`) +
            FfiConverterOptionalTypeFfiPaykitProfile.allocationSize(value.`paykitProfile`) +
            FfiConverterOptionalTypeFfiPubkyProfile.allocationSize(value.`pubkyProfile`) +
            FfiConverterString.allocationSize(value.`fetchedAt`)
    )

    override fun write(value: ContactProfileResolution, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypeContactProfileSource.write(value.`source`, buf)
        FfiConverterOptionalString.write(value.`displayName`, buf)
        FfiConverterOptionalString.write(value.`imageUri`, buf)
        FfiConverterOptionalTypeFfiPaykitProfile.write(value.`paykitProfile`, buf)
        FfiConverterOptionalTypeFfiPubkyProfile.write(value.`pubkyProfile`, buf)
        FfiConverterString.write(value.`fetchedAt`, buf)
    }
}




public object FfiConverterTypeContactRecord: FfiConverterRustBuffer<ContactRecord> {
    override fun read(buf: ByteBuffer): ContactRecord {
        return ContactRecord(
            FfiConverterString.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaykitProfile.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePublicationStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: ContactRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterSequenceString.allocationSize(value.`receiverPaths`) +
            FfiConverterOptionalString.allocationSize(value.`label`) +
            FfiConverterOptionalTypeFfiPaykitProfile.allocationSize(value.`profile`) +
            FfiConverterOptionalString.allocationSize(value.`profileFetchedAt`) +
            FfiConverterString.allocationSize(value.`createdAt`) +
            FfiConverterString.allocationSize(value.`updatedAt`) +
            FfiConverterTypePublicationStatus.allocationSize(value.`publicContactMarkerStatus`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactMarkerReceiverPath`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactPublishedAt`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactRemovedAt`) +
            FfiConverterOptionalString.allocationSize(value.`publicContactLastError`)
    )

    override fun write(value: ContactRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterSequenceString.write(value.`receiverPaths`, buf)
        FfiConverterOptionalString.write(value.`label`, buf)
        FfiConverterOptionalTypeFfiPaykitProfile.write(value.`profile`, buf)
        FfiConverterOptionalString.write(value.`profileFetchedAt`, buf)
        FfiConverterString.write(value.`createdAt`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
        FfiConverterTypePublicationStatus.write(value.`publicContactMarkerStatus`, buf)
        FfiConverterOptionalString.write(value.`publicContactMarkerReceiverPath`, buf)
        FfiConverterOptionalString.write(value.`publicContactPublishedAt`, buf)
        FfiConverterOptionalString.write(value.`publicContactRemovedAt`, buf)
        FfiConverterOptionalString.write(value.`publicContactLastError`, buf)
    }
}




public object FfiConverterTypeContactUpdate: FfiConverterRustBuffer<ContactUpdate> {
    override fun read(buf: ByteBuffer): ContactUpdate {
        return ContactUpdate(
            FfiConverterString.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: ContactUpdate): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterSequenceString.allocationSize(value.`receiverPaths`) +
            FfiConverterOptionalString.allocationSize(value.`label`)
    )

    override fun write(value: ContactUpdate, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterSequenceString.write(value.`receiverPaths`, buf)
        FfiConverterOptionalString.write(value.`label`, buf)
    }
}




public object FfiConverterTypeCounterpartyReceiver: FfiConverterRustBuffer<CounterpartyReceiver> {
    override fun read(buf: ByteBuffer): CounterpartyReceiver {
        return CounterpartyReceiver(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: CounterpartyReceiver): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`)
    )

    override fun write(value: CounterpartyReceiver, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
    }
}




public object FfiConverterTypeEncryptedLinkRecoveryMarkerReport: FfiConverterRustBuffer<EncryptedLinkRecoveryMarkerReport> {
    override fun read(buf: ByteBuffer): EncryptedLinkRecoveryMarkerReport {
        return EncryptedLinkRecoveryMarkerReport(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeLinkedPeerState.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: EncryptedLinkRecoveryMarkerReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterTypeLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterOptionalString.allocationSize(value.`localAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`localMarkerCreatedAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`localMarkerLastError`) +
            FfiConverterOptionalString.allocationSize(value.`remoteAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`remoteMarkerObservedAt`) +
            FfiConverterBoolean.allocationSize(value.`remoteMarkerChanged`)
    )

    override fun write(value: EncryptedLinkRecoveryMarkerReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterTypeLinkedPeerState.write(value.`state`, buf)
        FfiConverterOptionalString.write(value.`localAttemptId`, buf)
        FfiConverterOptionalString.write(value.`localMarkerCreatedAt`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`localMarkerLastError`, buf)
        FfiConverterOptionalString.write(value.`remoteAttemptId`, buf)
        FfiConverterOptionalString.write(value.`remoteMarkerObservedAt`, buf)
        FfiConverterBoolean.write(value.`remoteMarkerChanged`, buf)
    }
}




public object FfiConverterTypeEndpointSyncChange: FfiConverterRustBuffer<EndpointSyncChange> {
    override fun read(buf: ByteBuffer): EndpointSyncChange {
        return EndpointSyncChange(
            FfiConverterString.read(buf),
            FfiConverterTypePublicationStatus.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: EndpointSyncChange): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePublicationStatus.allocationSize(value.`status`) +
            FfiConverterOptionalString.allocationSize(value.`error`)
    )

    override fun write(value: EndpointSyncChange, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePublicationStatus.write(value.`status`, buf)
        FfiConverterOptionalString.write(value.`error`, buf)
    }
}




public object FfiConverterTypeEndpointSyncReport: FfiConverterRustBuffer<EndpointSyncReport> {
    override fun read(buf: ByteBuffer): EndpointSyncReport {
        return EndpointSyncReport(
            FfiConverterSequenceTypeEndpointSyncChange.read(buf),
            FfiConverterSequenceTypeEndpointSyncChange.read(buf),
            FfiConverterSequenceTypeEndpointSyncChange.read(buf),
        )
    }

    override fun allocationSize(value: EndpointSyncReport): ULong = (
            FfiConverterSequenceTypeEndpointSyncChange.allocationSize(value.`published`) +
            FfiConverterSequenceTypeEndpointSyncChange.allocationSize(value.`removed`) +
            FfiConverterSequenceTypeEndpointSyncChange.allocationSize(value.`failed`)
    )

    override fun write(value: EndpointSyncReport, buf: ByteBuffer) {
        FfiConverterSequenceTypeEndpointSyncChange.write(value.`published`, buf)
        FfiConverterSequenceTypeEndpointSyncChange.write(value.`removed`, buf)
        FfiConverterSequenceTypeEndpointSyncChange.write(value.`failed`, buf)
    }
}




public object FfiConverterTypeEventIdConflict: FfiConverterRustBuffer<EventIdConflict> {
    override fun read(buf: ByteBuffer): EventIdConflict {
        return EventIdConflict(
            FfiConverterString.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: EventIdConflict): ULong = (
            FfiConverterString.allocationSize(value.`eventId`) +
            FfiConverterULong.allocationSize(value.`firstStreamItemId`) +
            FfiConverterULong.allocationSize(value.`conflictingStreamItemId`)
    )

    override fun write(value: EventIdConflict, buf: ByteBuffer) {
        FfiConverterString.write(value.`eventId`, buf)
        FfiConverterULong.write(value.`firstStreamItemId`, buf)
        FfiConverterULong.write(value.`conflictingStreamItemId`, buf)
    }
}




public object FfiConverterTypeIdentityStatus: FfiConverterRustBuffer<IdentityStatus> {
    override fun read(buf: ByteBuffer): IdentityStatus {
        return IdentityStatus(
            FfiConverterOptionalString.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: IdentityStatus): ULong = (
            FfiConverterOptionalString.allocationSize(value.`publicKey`) +
            FfiConverterBoolean.allocationSize(value.`liveSessionAvailable`)
    )

    override fun write(value: IdentityStatus, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`publicKey`, buf)
        FfiConverterBoolean.write(value.`liveSessionAvailable`, buf)
    }
}




public object FfiConverterTypeInitializationReport: FfiConverterRustBuffer<InitializationReport> {
    override fun read(buf: ByteBuffer): InitializationReport {
        return InitializationReport(
            FfiConverterTypeIdentityStatus.read(buf),
        )
    }

    override fun allocationSize(value: InitializationReport): ULong = (
            FfiConverterTypeIdentityStatus.allocationSize(value.`identity`)
    )

    override fun write(value: InitializationReport, buf: ByteBuffer) {
        FfiConverterTypeIdentityStatus.write(value.`identity`, buf)
    }
}




public object FfiConverterTypeLinkedPeerHandshakeReport: FfiConverterRustBuffer<LinkedPeerHandshakeReport> {
    override fun read(buf: ByteBuffer): LinkedPeerHandshakeReport {
        return LinkedPeerHandshakeReport(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeLinkedPeerState.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.read(buf),
        )
    }

    override fun allocationSize(value: LinkedPeerHandshakeReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterTypeLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterULong.allocationSize(value.`generation`) +
            FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.allocationSize(value.`handshakeRole`)
    )

    override fun write(value: LinkedPeerHandshakeReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterTypeLinkedPeerState.write(value.`state`, buf)
        FfiConverterULong.write(value.`generation`, buf)
        FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole.write(value.`handshakeRole`, buf)
    }
}




public object FfiConverterTypeLinkedPeerRecord: FfiConverterRustBuffer<LinkedPeerRecord> {
    override fun read(buf: ByteBuffer): LinkedPeerRecord {
        return LinkedPeerRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeLinkedPeerState.read(buf),
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

    override fun allocationSize(value: LinkedPeerRecord): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterTypeLinkedPeerState.allocationSize(value.`state`) +
            FfiConverterOptionalString.allocationSize(value.`lastSyncAt`) +
            FfiConverterOptionalString.allocationSize(value.`lastPrivateReceiveAt`) +
            FfiConverterUInt.allocationSize(value.`failureCount`) +
            FfiConverterOptionalString.allocationSize(value.`localRecoveryAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`localRecoveryMarkerCreatedAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`localRecoveryMarkerLastError`) +
            FfiConverterOptionalString.allocationSize(value.`remoteRecoveryAttemptId`) +
            FfiConverterOptionalString.allocationSize(value.`remoteRecoveryMarkerObservedAt`)
    )

    override fun write(value: LinkedPeerRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterTypeLinkedPeerState.write(value.`state`, buf)
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




public object FfiConverterTypeOutboundPrivateCounterpartySendReport: FfiConverterRustBuffer<OutboundPrivateCounterpartySendReport> {
    override fun read(buf: ByteBuffer): OutboundPrivateCounterpartySendReport {
        return OutboundPrivateCounterpartySendReport(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: OutboundPrivateCounterpartySendReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.allocationSize(value.`report`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: OutboundPrivateCounterpartySendReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateSendReport.write(value.`report`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeOutboundPrivateSendFailure: FfiConverterRustBuffer<OutboundPrivateSendFailure> {
    override fun read(buf: ByteBuffer): OutboundPrivateSendFailure {
        return OutboundPrivateSendFailure(
            FfiConverterULong.read(buf),
            FfiConverterTypePrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: OutboundPrivateSendFailure): ULong = (
            FfiConverterULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterTypePrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: OutboundPrivateSendFailure, buf: ByteBuffer) {
        FfiConverterULong.write(value.`outboundMessageId`, buf)
        FfiConverterTypePrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeOutboundPrivateSendReport: FfiConverterRustBuffer<OutboundPrivateSendReport> {
    override fun read(buf: ByteBuffer): OutboundPrivateSendReport {
        return OutboundPrivateSendReport(
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceTypeOutboundPrivateSendFailure.read(buf),
            FfiConverterSequenceTypeReservationCleanupFailure.read(buf),
            FfiConverterSequenceTypeRecoveryMarkerPublishFailure.read(buf),
        )
    }

    override fun allocationSize(value: OutboundPrivateSendReport): ULong = (
            FfiConverterSequenceULong.allocationSize(value.`attempted`) +
            FfiConverterSequenceULong.allocationSize(value.`sent`) +
            FfiConverterSequenceTypeOutboundPrivateSendFailure.allocationSize(value.`failed`) +
            FfiConverterSequenceTypeReservationCleanupFailure.allocationSize(value.`reservationCleanupFailures`) +
            FfiConverterSequenceTypeRecoveryMarkerPublishFailure.allocationSize(value.`recoveryMarkerFailures`)
    )

    override fun write(value: OutboundPrivateSendReport, buf: ByteBuffer) {
        FfiConverterSequenceULong.write(value.`attempted`, buf)
        FfiConverterSequenceULong.write(value.`sent`, buf)
        FfiConverterSequenceTypeOutboundPrivateSendFailure.write(value.`failed`, buf)
        FfiConverterSequenceTypeReservationCleanupFailure.write(value.`reservationCleanupFailures`, buf)
        FfiConverterSequenceTypeRecoveryMarkerPublishFailure.write(value.`recoveryMarkerFailures`, buf)
    }
}




public object FfiConverterTypePaykitBlobRecord: FfiConverterRustBuffer<PaykitBlobRecord> {
    override fun read(buf: ByteBuffer): PaykitBlobRecord {
        return PaykitBlobRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaykitBlobRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`uri`) +
            FfiConverterULong.allocationSize(value.`sizeBytes`) +
            FfiConverterString.allocationSize(value.`updatedAt`)
    )

    override fun write(value: PaykitBlobRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`uri`, buf)
        FfiConverterULong.write(value.`sizeBytes`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
    }
}




public object FfiConverterTypePaykitProfile: FfiConverterRustBuffer<PaykitProfile> {
    override fun read(buf: ByteBuffer): PaykitProfile {
        return PaykitProfile(
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PaykitProfile): ULong = (
            FfiConverterOptionalString.allocationSize(value.`displayName`) +
            FfiConverterOptionalString.allocationSize(value.`imageUri`) +
            FfiConverterOptionalString.allocationSize(value.`extraJson`)
    )

    override fun write(value: PaykitProfile, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`displayName`, buf)
        FfiConverterOptionalString.write(value.`imageUri`, buf)
        FfiConverterOptionalString.write(value.`extraJson`, buf)
    }
}




public object FfiConverterTypePaykitProfileRecord: FfiConverterRustBuffer<PaykitProfileRecord> {
    override fun read(buf: ByteBuffer): PaykitProfileRecord {
        return PaykitProfileRecord(
            FfiConverterString.read(buf),
            FfiConverterTypePaykitProfile.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaykitProfileRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypePaykitProfile.allocationSize(value.`profile`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`updatedAt`)
    )

    override fun write(value: PaykitProfileRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypePaykitProfile.write(value.`profile`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
    }
}




public object FfiConverterTypePaykitReceiverCapabilities: FfiConverterRustBuffer<PaykitReceiverCapabilities> {
    override fun read(buf: ByteBuffer): PaykitReceiverCapabilities {
        return PaykitReceiverCapabilities(
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: PaykitReceiverCapabilities): ULong = (
            FfiConverterBoolean.allocationSize(value.`privatePayments`) +
            FfiConverterBoolean.allocationSize(value.`paymentRequests`) +
            FfiConverterBoolean.allocationSize(value.`receipts`) +
            FfiConverterBoolean.allocationSize(value.`outgoingPayments`)
    )

    override fun write(value: PaykitReceiverCapabilities, buf: ByteBuffer) {
        FfiConverterBoolean.write(value.`privatePayments`, buf)
        FfiConverterBoolean.write(value.`paymentRequests`, buf)
        FfiConverterBoolean.write(value.`receipts`, buf)
        FfiConverterBoolean.write(value.`outgoingPayments`, buf)
    }
}




public object FfiConverterTypePaykitReceiverMarker: FfiConverterRustBuffer<PaykitReceiverMarker> {
    override fun read(buf: ByteBuffer): PaykitReceiverMarker {
        return PaykitReceiverMarker(
            FfiConverterString.read(buf),
            FfiConverterTypePaykitReceiverCapabilities.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaykitReceiverMarker): ULong = (
            FfiConverterString.allocationSize(value.`receiverPath`) +
            FfiConverterTypePaykitReceiverCapabilities.allocationSize(value.`capabilities`) +
            FfiConverterString.allocationSize(value.`noisePublicKey`)
    )

    override fun write(value: PaykitReceiverMarker, buf: ByteBuffer) {
        FfiConverterString.write(value.`receiverPath`, buf)
        FfiConverterTypePaykitReceiverCapabilities.write(value.`capabilities`, buf)
        FfiConverterString.write(value.`noisePublicKey`, buf)
    }
}




public object FfiConverterTypePaykitSdkConfig: FfiConverterRustBuffer<PaykitSdkConfig> {
    override fun read(buf: ByteBuffer): PaykitSdkConfig {
        return PaykitSdkConfig(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeEndpointManagementScope.read(buf),
            FfiConverterTypeEncryptedLinkRecoveryMarkerPolicy.read(buf),
            FfiConverterTypePublicContactSharingPolicy.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: PaykitSdkConfig): ULong = (
            FfiConverterString.allocationSize(value.`receiverPath`) +
            FfiConverterString.allocationSize(value.`profileNamespace`) +
            FfiConverterTypeEndpointManagementScope.allocationSize(value.`endpointManagementScope`) +
            FfiConverterTypeEncryptedLinkRecoveryMarkerPolicy.allocationSize(value.`encryptedLinkRecoveryMarkers`) +
            FfiConverterTypePublicContactSharingPolicy.allocationSize(value.`publicContactSharing`) +
            FfiConverterULong.allocationSize(value.`peerLinkOperationLeaseTimeoutSecs`) +
            FfiConverterULong.allocationSize(value.`outboundPrivateSendLeaseTimeoutSecs`) +
            FfiConverterULong.allocationSize(value.`outboundPrivateRetryBackoffSecs`)
    )

    override fun write(value: PaykitSdkConfig, buf: ByteBuffer) {
        FfiConverterString.write(value.`receiverPath`, buf)
        FfiConverterString.write(value.`profileNamespace`, buf)
        FfiConverterTypeEndpointManagementScope.write(value.`endpointManagementScope`, buf)
        FfiConverterTypeEncryptedLinkRecoveryMarkerPolicy.write(value.`encryptedLinkRecoveryMarkers`, buf)
        FfiConverterTypePublicContactSharingPolicy.write(value.`publicContactSharing`, buf)
        FfiConverterULong.write(value.`peerLinkOperationLeaseTimeoutSecs`, buf)
        FfiConverterULong.write(value.`outboundPrivateSendLeaseTimeoutSecs`, buf)
        FfiConverterULong.write(value.`outboundPrivateRetryBackoffSecs`, buf)
    }
}




public object FfiConverterTypePaymentAmountContext: FfiConverterRustBuffer<PaymentAmountContext> {
    override fun read(buf: ByteBuffer): PaymentAmountContext {
        return PaymentAmountContext(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaymentAmountContext): ULong = (
            FfiConverterString.allocationSize(value.`value`) +
            FfiConverterString.allocationSize(value.`asset`)
    )

    override fun write(value: PaymentAmountContext, buf: ByteBuffer) {
        FfiConverterString.write(value.`value`, buf)
        FfiConverterString.write(value.`asset`, buf)
    }
}




public object FfiConverterTypePaymentProofRecord: FfiConverterRustBuffer<PaymentProofRecord> {
    override fun read(buf: ByteBuffer): PaymentProofRecord {
        return PaymentProofRecord(
            FfiConverterString.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePrivateJsonObject.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaymentProofRecord): ULong = (
            FfiConverterString.allocationSize(value.`eventId`) +
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`outboundStatus`) +
            FfiConverterOptionalULong.allocationSize(value.`streamItemId`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterString.allocationSize(value.`paymentEndpointIdentifier`) +
            FfiConverterTypePrivateJsonObject.allocationSize(value.`proof`) +
            FfiConverterString.allocationSize(value.`recordedAt`)
    )

    override fun write(value: PaymentProofRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`eventId`, buf)
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`outboundStatus`, buf)
        FfiConverterOptionalULong.write(value.`streamItemId`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterString.write(value.`paymentEndpointIdentifier`, buf)
        FfiConverterTypePrivateJsonObject.write(value.`proof`, buf)
        FfiConverterString.write(value.`recordedAt`, buf)
    }
}




public object FfiConverterTypePaymentProofSubmission: FfiConverterRustBuffer<PaymentProofSubmission> {
    override fun read(buf: ByteBuffer): PaymentProofSubmission {
        return PaymentProofSubmission(
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePrivateJsonObject.read(buf),
        )
    }

    override fun allocationSize(value: PaymentProofSubmission): ULong = (
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterString.allocationSize(value.`paymentEndpointIdentifier`) +
            FfiConverterTypePrivateJsonObject.allocationSize(value.`proof`)
    )

    override fun write(value: PaymentProofSubmission, buf: ByteBuffer) {
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterString.write(value.`paymentEndpointIdentifier`, buf)
        FfiConverterTypePrivateJsonObject.write(value.`proof`, buf)
    }
}




public object FfiConverterTypePaymentRequestAmount: FfiConverterRustBuffer<PaymentRequestAmount> {
    override fun read(buf: ByteBuffer): PaymentRequestAmount {
        return PaymentRequestAmount(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PaymentRequestAmount): ULong = (
            FfiConverterString.allocationSize(value.`value`) +
            FfiConverterString.allocationSize(value.`asset`)
    )

    override fun write(value: PaymentRequestAmount, buf: ByteBuffer) {
        FfiConverterString.write(value.`value`, buf)
        FfiConverterString.write(value.`asset`, buf)
    }
}




public object FfiConverterTypePaymentRequestFilter: FfiConverterRustBuffer<PaymentRequestFilter> {
    override fun read(buf: ByteBuffer): PaymentRequestFilter {
        return PaymentRequestFilter(
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaymentRequestLocalRole.read(buf),
            FfiConverterSequenceTypePaymentRequestLifecycleState.read(buf),
            FfiConverterOptionalBoolean.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: PaymentRequestFilter): ULong = (
            FfiConverterOptionalString.allocationSize(value.`counterparty`) +
            FfiConverterOptionalString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiPaymentRequestLocalRole.allocationSize(value.`localRole`) +
            FfiConverterSequenceTypePaymentRequestLifecycleState.allocationSize(value.`states`) +
            FfiConverterOptionalBoolean.allocationSize(value.`recurring`) +
            FfiConverterBoolean.allocationSize(value.`receivedOnly`)
    )

    override fun write(value: PaymentRequestFilter, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`counterparty`, buf)
        FfiConverterOptionalString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiPaymentRequestLocalRole.write(value.`localRole`, buf)
        FfiConverterSequenceTypePaymentRequestLifecycleState.write(value.`states`, buf)
        FfiConverterOptionalBoolean.write(value.`recurring`, buf)
        FfiConverterBoolean.write(value.`receivedOnly`, buf)
    }
}




public object FfiConverterTypePaymentRequestRecord: FfiConverterRustBuffer<PaymentRequestRecord> {
    override fun read(buf: ByteBuffer): PaymentRequestRecord {
        return PaymentRequestRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPaymentRequestLocalRole.read(buf),
            FfiConverterTypePaymentRequestLifecycleState.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaymentRequestTerms.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterSequenceTypePaymentProofRecord.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PaymentRequestRecord): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`paymentRequestId`) +
            FfiConverterOptionalTypeFfiPaymentRequestLocalRole.allocationSize(value.`localRole`) +
            FfiConverterTypePaymentRequestLifecycleState.allocationSize(value.`state`) +
            FfiConverterOptionalULong.allocationSize(value.`proposalStreamItemId`) +
            FfiConverterOptionalULong.allocationSize(value.`proposalOutboundMessageId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`proposalOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`proposalEventId`) +
            FfiConverterOptionalTypeFfiPaymentRequestTerms.allocationSize(value.`terms`) +
            FfiConverterOptionalString.allocationSize(value.`acceptedEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`acceptedOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`rejectedEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`rejectedOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`canceledEventId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`canceledOutboundStatus`) +
            FfiConverterSequenceTypePaymentProofRecord.allocationSize(value.`paymentProofs`) +
            FfiConverterOptionalULong.allocationSize(value.`lastStreamItemId`) +
            FfiConverterOptionalULong.allocationSize(value.`lastOutboundMessageId`) +
            FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.allocationSize(value.`lastOutboundStatus`) +
            FfiConverterOptionalString.allocationSize(value.`lastEventAt`) +
            FfiConverterOptionalString.allocationSize(value.`invalidReason`)
    )

    override fun write(value: PaymentRequestRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`paymentRequestId`, buf)
        FfiConverterOptionalTypeFfiPaymentRequestLocalRole.write(value.`localRole`, buf)
        FfiConverterTypePaymentRequestLifecycleState.write(value.`state`, buf)
        FfiConverterOptionalULong.write(value.`proposalStreamItemId`, buf)
        FfiConverterOptionalULong.write(value.`proposalOutboundMessageId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`proposalOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`proposalEventId`, buf)
        FfiConverterOptionalTypeFfiPaymentRequestTerms.write(value.`terms`, buf)
        FfiConverterOptionalString.write(value.`acceptedEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`acceptedOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`rejectedEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`rejectedOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`canceledEventId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`canceledOutboundStatus`, buf)
        FfiConverterSequenceTypePaymentProofRecord.write(value.`paymentProofs`, buf)
        FfiConverterOptionalULong.write(value.`lastStreamItemId`, buf)
        FfiConverterOptionalULong.write(value.`lastOutboundMessageId`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus.write(value.`lastOutboundStatus`, buf)
        FfiConverterOptionalString.write(value.`lastEventAt`, buf)
        FfiConverterOptionalString.write(value.`invalidReason`, buf)
    }
}




public object FfiConverterTypePaymentRequestRecurrence: FfiConverterRustBuffer<PaymentRequestRecurrence> {
    override fun read(buf: ByteBuffer): PaymentRequestRecurrence {
        return PaymentRequestRecurrence(
            FfiConverterUInt.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PaymentRequestRecurrence): ULong = (
            FfiConverterUInt.allocationSize(value.`every`) +
            FfiConverterString.allocationSize(value.`unit`) +
            FfiConverterString.allocationSize(value.`startsAt`) +
            FfiConverterString.allocationSize(value.`anchor`) +
            FfiConverterOptionalString.allocationSize(value.`endsAt`)
    )

    override fun write(value: PaymentRequestRecurrence, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`every`, buf)
        FfiConverterString.write(value.`unit`, buf)
        FfiConverterString.write(value.`startsAt`, buf)
        FfiConverterString.write(value.`anchor`, buf)
        FfiConverterOptionalString.write(value.`endsAt`, buf)
    }
}




public object FfiConverterTypePaymentRequestTerms: FfiConverterRustBuffer<PaymentRequestTerms> {
    override fun read(buf: ByteBuffer): PaymentRequestTerms {
        return PaymentRequestTerms(
            FfiConverterTypePaymentRequestAmount.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPaymentRequestRecurrence.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterTypePrivateJsonObject.read(buf),
        )
    }

    override fun allocationSize(value: PaymentRequestTerms): ULong = (
            FfiConverterTypePaymentRequestAmount.allocationSize(value.`amount`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalString.allocationSize(value.`proposalExpiresAt`) +
            FfiConverterOptionalTypeFfiPaymentRequestRecurrence.allocationSize(value.`recurrence`) +
            FfiConverterSequenceString.allocationSize(value.`acceptedPaymentEndpointIdentifiers`) +
            FfiConverterTypePrivateJsonObject.allocationSize(value.`metadata`)
    )

    override fun write(value: PaymentRequestTerms, buf: ByteBuffer) {
        FfiConverterTypePaymentRequestAmount.write(value.`amount`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalString.write(value.`proposalExpiresAt`, buf)
        FfiConverterOptionalTypeFfiPaymentRequestRecurrence.write(value.`recurrence`, buf)
        FfiConverterSequenceString.write(value.`acceptedPaymentEndpointIdentifiers`, buf)
        FfiConverterTypePrivateJsonObject.write(value.`metadata`, buf)
    }
}




public object FfiConverterTypePaymentTarget: FfiConverterRustBuffer<PaymentTarget> {
    override fun read(buf: ByteBuffer): PaymentTarget {
        return PaymentTarget(
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PaymentTarget): ULong = (
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PaymentTarget, buf: ByteBuffer) {
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypePreparedPrivateContactPayment: FfiConverterRustBuffer<PreparedPrivateContactPayment> {
    override fun read(buf: ByteBuffer): PreparedPrivateContactPayment {
        return PreparedPrivateContactPayment(
            FfiConverterTypePrivateContactPaymentResolution.read(buf),
            FfiConverterOptionalTypeFfiLinkedPeerHandshakeReport.read(buf),
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.read(buf),
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.read(buf),
        )
    }

    override fun allocationSize(value: PreparedPrivateContactPayment): ULong = (
            FfiConverterTypePrivateContactPaymentResolution.allocationSize(value.`resolution`) +
            FfiConverterOptionalTypeFfiLinkedPeerHandshakeReport.allocationSize(value.`linkReport`) +
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.allocationSize(value.`receiveReport`) +
            FfiConverterOptionalTypeFfiOutboundPrivateSendReport.allocationSize(value.`outboundReport`)
    )

    override fun write(value: PreparedPrivateContactPayment, buf: ByteBuffer) {
        FfiConverterTypePrivateContactPaymentResolution.write(value.`resolution`, buf)
        FfiConverterOptionalTypeFfiLinkedPeerHandshakeReport.write(value.`linkReport`, buf)
        FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.write(value.`receiveReport`, buf)
        FfiConverterOptionalTypeFfiOutboundPrivateSendReport.write(value.`outboundReport`, buf)
    }
}




public object FfiConverterTypePrivateContactPaymentResolution: FfiConverterRustBuffer<PrivateContactPaymentResolution> {
    override fun read(buf: ByteBuffer): PrivateContactPaymentResolution {
        return PrivateContactPaymentResolution(
            FfiConverterTypePrivatePaymentResolutionStatus.read(buf),
            FfiConverterTypePrivatePaymentResolutionState.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterSequenceTypeResolvedPrivatePaymentEndpoint.read(buf),
        )
    }

    override fun allocationSize(value: PrivateContactPaymentResolution): ULong = (
            FfiConverterTypePrivatePaymentResolutionStatus.allocationSize(value.`status`) +
            FfiConverterTypePrivatePaymentResolutionState.allocationSize(value.`state`) +
            FfiConverterOptionalULong.allocationSize(value.`privatePaymentListVersion`) +
            FfiConverterSequenceTypeResolvedPrivatePaymentEndpoint.allocationSize(value.`payableEndpoints`)
    )

    override fun write(value: PrivateContactPaymentResolution, buf: ByteBuffer) {
        FfiConverterTypePrivatePaymentResolutionStatus.write(value.`status`, buf)
        FfiConverterTypePrivatePaymentResolutionState.write(value.`state`, buf)
        FfiConverterOptionalULong.write(value.`privatePaymentListVersion`, buf)
        FfiConverterSequenceTypeResolvedPrivatePaymentEndpoint.write(value.`payableEndpoints`, buf)
    }
}




public object FfiConverterTypePrivatePaymentEndpointCandidate: FfiConverterRustBuffer<PrivatePaymentEndpointCandidate> {
    override fun read(buf: ByteBuffer): PrivatePaymentEndpointCandidate {
        return PrivatePaymentEndpointCandidate(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentEndpointCandidate): ULong = (
            FfiConverterString.allocationSize(value.`candidateId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PrivatePaymentEndpointCandidate, buf: ByteBuffer) {
        FfiConverterString.write(value.`candidateId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypePrivatePaymentEndpointReservation: FfiConverterRustBuffer<PrivatePaymentEndpointReservation> {
    override fun read(buf: ByteBuffer): PrivatePaymentEndpointReservation {
        return PrivatePaymentEndpointReservation(
            FfiConverterString.read(buf),
            FfiConverterTypePrivateReceivingDetail.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterTypeReservationAttribution.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentEndpointReservation): ULong = (
            FfiConverterString.allocationSize(value.`reservationId`) +
            FfiConverterTypePrivateReceivingDetail.allocationSize(value.`receivingDetail`) +
            FfiConverterOptionalString.allocationSize(value.`expiresAt`) +
            FfiConverterTypeReservationAttribution.allocationSize(value.`attribution`)
    )

    override fun write(value: PrivatePaymentEndpointReservation, buf: ByteBuffer) {
        FfiConverterString.write(value.`reservationId`, buf)
        FfiConverterTypePrivateReceivingDetail.write(value.`receivingDetail`, buf)
        FfiConverterOptionalString.write(value.`expiresAt`, buf)
        FfiConverterTypeReservationAttribution.write(value.`attribution`, buf)
    }
}




public object FfiConverterTypePrivatePaymentEndpointReservationCancellation: FfiConverterRustBuffer<PrivatePaymentEndpointReservationCancellation> {
    override fun read(buf: ByteBuffer): PrivatePaymentEndpointReservationCancellation {
        return PrivatePaymentEndpointReservationCancellation(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeReservationAttribution.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentEndpointReservationCancellation): ULong = (
            FfiConverterString.allocationSize(value.`reservationId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterString.allocationSize(value.`payloadHash`) +
            FfiConverterTypeReservationAttribution.allocationSize(value.`attribution`)
    )

    override fun write(value: PrivatePaymentEndpointReservationCancellation, buf: ByteBuffer) {
        FfiConverterString.write(value.`reservationId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterString.write(value.`payloadHash`, buf)
        FfiConverterTypeReservationAttribution.write(value.`attribution`, buf)
    }
}




public object FfiConverterTypePrivatePaymentEndpointReservationInput: FfiConverterRustBuffer<PrivatePaymentEndpointReservationInput> {
    override fun read(buf: ByteBuffer): PrivatePaymentEndpointReservationInput {
        return PrivatePaymentEndpointReservationInput(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterMapStringString.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentEndpointReservationInput): ULong = (
            FfiConverterString.allocationSize(value.`reservationId`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterString.allocationSize(value.`payload`) +
            FfiConverterOptionalString.allocationSize(value.`expiresAt`) +
            FfiConverterMapStringString.allocationSize(value.`attribution`)
    )

    override fun write(value: PrivatePaymentEndpointReservationInput, buf: ByteBuffer) {
        FfiConverterString.write(value.`reservationId`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterString.write(value.`payload`, buf)
        FfiConverterOptionalString.write(value.`expiresAt`, buf)
        FfiConverterMapStringString.write(value.`attribution`, buf)
    }
}




public object FfiConverterTypePrivatePaymentEndpointSelectionRequest: FfiConverterRustBuffer<PrivatePaymentEndpointSelectionRequest> {
    override fun read(buf: ByteBuffer): PrivatePaymentEndpointSelectionRequest {
        return PrivatePaymentEndpointSelectionRequest(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPaymentAmountContext.read(buf),
            FfiConverterSequenceTypePrivatePaymentEndpointCandidate.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentEndpointSelectionRequest): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiPaymentAmountContext.allocationSize(value.`amount`) +
            FfiConverterSequenceTypePrivatePaymentEndpointCandidate.allocationSize(value.`candidates`)
    )

    override fun write(value: PrivatePaymentEndpointSelectionRequest, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiPaymentAmountContext.write(value.`amount`, buf)
        FfiConverterSequenceTypePrivatePaymentEndpointCandidate.write(value.`candidates`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListDeliveryFailure: FfiConverterRustBuffer<PrivatePaymentListDeliveryFailure> {
    override fun read(buf: ByteBuffer): PrivatePaymentListDeliveryFailure {
        return PrivatePaymentListDeliveryFailure(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterTypePrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListDeliveryFailure): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterOptionalString.allocationSize(value.`reservationId`) +
            FfiConverterTypePrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: PrivatePaymentListDeliveryFailure, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterOptionalString.write(value.`reservationId`, buf)
        FfiConverterTypePrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListDeliveryReport: FfiConverterRustBuffer<PrivatePaymentListDeliveryReport> {
    override fun read(buf: ByteBuffer): PrivatePaymentListDeliveryReport {
        return PrivatePaymentListDeliveryReport(
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
            FfiConverterSequenceTypePrivatePaymentListDeliveryFailure.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListDeliveryReport): ULong = (
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`queued`) +
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`cleared`) +
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`failedToQueue`) +
            FfiConverterSequenceTypePrivatePaymentListDeliveryFailure.allocationSize(value.`failedToDeliver`)
    )

    override fun write(value: PrivatePaymentListDeliveryReport, buf: ByteBuffer) {
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`queued`, buf)
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`cleared`, buf)
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`failedToQueue`, buf)
        FfiConverterSequenceTypePrivatePaymentListDeliveryFailure.write(value.`failedToDeliver`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListEndpoint: FfiConverterRustBuffer<PrivatePaymentListEndpoint> {
    override fun read(buf: ByteBuffer): PrivatePaymentListEndpoint {
        return PrivatePaymentListEndpoint(
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListEndpoint): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PrivatePaymentListEndpoint, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListReservationUpdateInput: FfiConverterRustBuffer<PrivatePaymentListReservationUpdateInput> {
    override fun read(buf: ByteBuffer): PrivatePaymentListReservationUpdateInput {
        return PrivatePaymentListReservationUpdateInput(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterSequenceTypePrivatePaymentEndpointReservationInput.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListReservationUpdateInput): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterSequenceTypePrivatePaymentEndpointReservationInput.allocationSize(value.`reservations`)
    )

    override fun write(value: PrivatePaymentListReservationUpdateInput, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterSequenceTypePrivatePaymentEndpointReservationInput.write(value.`reservations`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListSyncChange: FfiConverterRustBuffer<PrivatePaymentListSyncChange> {
    override fun read(buf: ByteBuffer): PrivatePaymentListSyncChange {
        return PrivatePaymentListSyncChange(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListSyncChange): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterOptionalString.allocationSize(value.`error`)
    )

    override fun write(value: PrivatePaymentListSyncChange, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterOptionalString.write(value.`error`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListSyncReport: FfiConverterRustBuffer<PrivatePaymentListSyncReport> {
    override fun read(buf: ByteBuffer): PrivatePaymentListSyncReport {
        return PrivatePaymentListSyncReport(
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
            FfiConverterSequenceTypePrivatePaymentListSyncChange.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListSyncReport): ULong = (
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`queued`) +
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`cleared`) +
            FfiConverterSequenceTypePrivatePaymentListSyncChange.allocationSize(value.`failed`)
    )

    override fun write(value: PrivatePaymentListSyncReport, buf: ByteBuffer) {
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`queued`, buf)
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`cleared`, buf)
        FfiConverterSequenceTypePrivatePaymentListSyncChange.write(value.`failed`, buf)
    }
}




public object FfiConverterTypePrivatePaymentListView: FfiConverterRustBuffer<PrivatePaymentListView> {
    override fun read(buf: ByteBuffer): PrivatePaymentListView {
        return PrivatePaymentListView(
            FfiConverterOptionalULong.read(buf),
            FfiConverterSequenceTypePrivatePaymentListEndpoint.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PrivatePaymentListView): ULong = (
            FfiConverterOptionalULong.allocationSize(value.`latestStreamItemId`) +
            FfiConverterSequenceTypePrivatePaymentListEndpoint.allocationSize(value.`paymentEndpoints`) +
            FfiConverterOptionalString.allocationSize(value.`lastRefreshAt`)
    )

    override fun write(value: PrivatePaymentListView, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`latestStreamItemId`, buf)
        FfiConverterSequenceTypePrivatePaymentListEndpoint.write(value.`paymentEndpoints`, buf)
        FfiConverterOptionalString.write(value.`lastRefreshAt`, buf)
    }
}




public object FfiConverterTypePrivateReceivingDetail: FfiConverterRustBuffer<PrivateReceivingDetail> {
    override fun read(buf: ByteBuffer): PrivateReceivingDetail {
        return PrivateReceivingDetail(
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PrivateReceivingDetail): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PrivateReceivingDetail, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypePrivateReceivingDetailReservationResponse: FfiConverterRustBuffer<PrivateReceivingDetailReservationResponse> {
    override fun read(buf: ByteBuffer): PrivateReceivingDetailReservationResponse {
        return PrivateReceivingDetailReservationResponse(
            FfiConverterTypePrivateReceivingDetailReservationResponseKind.read(buf),
            FfiConverterSequenceTypePrivatePaymentEndpointReservation.read(buf),
        )
    }

    override fun allocationSize(value: PrivateReceivingDetailReservationResponse): ULong = (
            FfiConverterTypePrivateReceivingDetailReservationResponseKind.allocationSize(value.`kind`) +
            FfiConverterSequenceTypePrivatePaymentEndpointReservation.allocationSize(value.`reservations`)
    )

    override fun write(value: PrivateReceivingDetailReservationResponse, buf: ByteBuffer) {
        FfiConverterTypePrivateReceivingDetailReservationResponseKind.write(value.`kind`, buf)
        FfiConverterSequenceTypePrivatePaymentEndpointReservation.write(value.`reservations`, buf)
    }
}




public object FfiConverterTypePrivateStreamCounterpartyIntakeReport: FfiConverterRustBuffer<PrivateStreamCounterpartyIntakeReport> {
    override fun read(buf: ByteBuffer): PrivateStreamCounterpartyIntakeReport {
        return PrivateStreamCounterpartyIntakeReport(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: PrivateStreamCounterpartyIntakeReport): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.allocationSize(value.`report`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: PrivateStreamCounterpartyIntakeReport, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiPrivateStreamIntakeReport.write(value.`report`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypePrivateStreamIntakeReport: FfiConverterRustBuffer<PrivateStreamIntakeReport> {
    override fun read(buf: ByteBuffer): PrivateStreamIntakeReport {
        return PrivateStreamIntakeReport(
            FfiConverterULong.read(buf),
            FfiConverterSequenceULong.read(buf),
            FfiConverterSequenceTypeEventIdConflict.read(buf),
        )
    }

    override fun allocationSize(value: PrivateStreamIntakeReport): ULong = (
            FfiConverterULong.allocationSize(value.`receiveBatchId`) +
            FfiConverterSequenceULong.allocationSize(value.`streamItemIds`) +
            FfiConverterSequenceTypeEventIdConflict.allocationSize(value.`eventConflicts`)
    )

    override fun write(value: PrivateStreamIntakeReport, buf: ByteBuffer) {
        FfiConverterULong.write(value.`receiveBatchId`, buf)
        FfiConverterSequenceULong.write(value.`streamItemIds`, buf)
        FfiConverterSequenceTypeEventIdConflict.write(value.`eventConflicts`, buf)
    }
}




public object FfiConverterTypePubkyAuthCompanionClaim: FfiConverterRustBuffer<PubkyAuthCompanionClaim> {
    override fun read(buf: ByteBuffer): PubkyAuthCompanionClaim {
        return PubkyAuthCompanionClaim(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterByteArray.read(buf),
        )
    }

    override fun allocationSize(value: PubkyAuthCompanionClaim): ULong = (
            FfiConverterString.allocationSize(value.`queryParameter`) +
            FfiConverterString.allocationSize(value.`claimType`) +
            FfiConverterByteArray.allocationSize(value.`unsignedPayload`)
    )

    override fun write(value: PubkyAuthCompanionClaim, buf: ByteBuffer) {
        FfiConverterString.write(value.`queryParameter`, buf)
        FfiConverterString.write(value.`claimType`, buf)
        FfiConverterByteArray.write(value.`unsignedPayload`, buf)
    }
}




public object FfiConverterTypePubkyAuthDetails: FfiConverterRustBuffer<PubkyAuthDetails> {
    override fun read(buf: ByteBuffer): PubkyAuthDetails {
        return PubkyAuthDetails(
            FfiConverterTypePubkyAuthRequestKind.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyAuthDetails): ULong = (
            FfiConverterTypePubkyAuthRequestKind.allocationSize(value.`kind`) +
            FfiConverterOptionalString.allocationSize(value.`capabilities`) +
            FfiConverterOptionalString.allocationSize(value.`relayUrl`) +
            FfiConverterOptionalString.allocationSize(value.`homeserverPublicKey`)
    )

    override fun write(value: PubkyAuthDetails, buf: ByteBuffer) {
        FfiConverterTypePubkyAuthRequestKind.write(value.`kind`, buf)
        FfiConverterOptionalString.write(value.`capabilities`, buf)
        FfiConverterOptionalString.write(value.`relayUrl`, buf)
        FfiConverterOptionalString.write(value.`homeserverPublicKey`, buf)
    }
}




public object FfiConverterTypePubkyClientConfig: FfiConverterRustBuffer<PubkyClientConfig> {
    override fun read(buf: ByteBuffer): PubkyClientConfig {
        return PubkyClientConfig(
            FfiConverterULong.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyClientConfig): ULong = (
            FfiConverterULong.allocationSize(value.`requestTimeoutSecs`) +
            FfiConverterOptionalString.allocationSize(value.`localTestnetHost`)
    )

    override fun write(value: PubkyClientConfig, buf: ByteBuffer) {
        FfiConverterULong.write(value.`requestTimeoutSecs`, buf)
        FfiConverterOptionalString.write(value.`localTestnetHost`, buf)
    }
}




public object FfiConverterTypePubkyProfile: FfiConverterRustBuffer<PubkyProfile> {
    override fun read(buf: ByteBuffer): PubkyProfile {
        return PubkyProfile(
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterSequenceTypePubkyProfileLink.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyProfile): ULong = (
            FfiConverterString.allocationSize(value.`name`) +
            FfiConverterOptionalString.allocationSize(value.`bio`) +
            FfiConverterOptionalString.allocationSize(value.`image`) +
            FfiConverterSequenceTypePubkyProfileLink.allocationSize(value.`links`) +
            FfiConverterOptionalString.allocationSize(value.`status`)
    )

    override fun write(value: PubkyProfile, buf: ByteBuffer) {
        FfiConverterString.write(value.`name`, buf)
        FfiConverterOptionalString.write(value.`bio`, buf)
        FfiConverterOptionalString.write(value.`image`, buf)
        FfiConverterSequenceTypePubkyProfileLink.write(value.`links`, buf)
        FfiConverterOptionalString.write(value.`status`, buf)
    }
}




public object FfiConverterTypePubkyProfileLink: FfiConverterRustBuffer<PubkyProfileLink> {
    override fun read(buf: ByteBuffer): PubkyProfileLink {
        return PubkyProfileLink(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyProfileLink): ULong = (
            FfiConverterString.allocationSize(value.`title`) +
            FfiConverterString.allocationSize(value.`url`)
    )

    override fun write(value: PubkyProfileLink, buf: ByteBuffer) {
        FfiConverterString.write(value.`title`, buf)
        FfiConverterString.write(value.`url`, buf)
    }
}




public object FfiConverterTypePubkyProfileRecord: FfiConverterRustBuffer<PubkyProfileRecord> {
    override fun read(buf: ByteBuffer): PubkyProfileRecord {
        return PubkyProfileRecord(
            FfiConverterString.read(buf),
            FfiConverterTypePubkyProfile.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyProfileRecord): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterTypePubkyProfile.allocationSize(value.`profile`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`fetchedAt`)
    )

    override fun write(value: PubkyProfileRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterTypePubkyProfile.write(value.`profile`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`fetchedAt`, buf)
    }
}




public object FfiConverterTypePubkyResourceRef: FfiConverterRustBuffer<PubkyResourceRef> {
    override fun read(buf: ByteBuffer): PubkyResourceRef {
        return PubkyResourceRef(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PubkyResourceRef): ULong = (
            FfiConverterString.allocationSize(value.`publicKey`) +
            FfiConverterString.allocationSize(value.`path`) +
            FfiConverterString.allocationSize(value.`transportUrl`)
    )

    override fun write(value: PubkyResourceRef, buf: ByteBuffer) {
        FfiConverterString.write(value.`publicKey`, buf)
        FfiConverterString.write(value.`path`, buf)
        FfiConverterString.write(value.`transportUrl`, buf)
    }
}




public object FfiConverterTypePubkySessionBootstrapResult: FfiConverterRustBuffer<PubkySessionBootstrapResult> {
    override fun read(buf: ByteBuffer): PubkySessionBootstrapResult {
        return PubkySessionBootstrapResult(
            FfiConverterTypePubkySessionAccess.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: PubkySessionBootstrapResult): ULong = (
            FfiConverterTypePubkySessionAccess.allocationSize(value.`sessionAccess`) +
            FfiConverterString.allocationSize(value.`publicKey`)
    )

    override fun write(value: PubkySessionBootstrapResult, buf: ByteBuffer) {
        FfiConverterTypePubkySessionAccess.write(value.`sessionAccess`, buf)
        FfiConverterString.write(value.`publicKey`, buf)
    }
}




public object FfiConverterTypePublicContactPaymentResolution: FfiConverterRustBuffer<PublicContactPaymentResolution> {
    override fun read(buf: ByteBuffer): PublicContactPaymentResolution {
        return PublicContactPaymentResolution(
            FfiConverterTypePublicPaymentResolutionStatus.read(buf),
            FfiConverterSequenceTypeResolvedPublicPaymentEndpoint.read(buf),
        )
    }

    override fun allocationSize(value: PublicContactPaymentResolution): ULong = (
            FfiConverterTypePublicPaymentResolutionStatus.allocationSize(value.`status`) +
            FfiConverterSequenceTypeResolvedPublicPaymentEndpoint.allocationSize(value.`payableEndpoints`)
    )

    override fun write(value: PublicContactPaymentResolution, buf: ByteBuffer) {
        FfiConverterTypePublicPaymentResolutionStatus.write(value.`status`, buf)
        FfiConverterSequenceTypeResolvedPublicPaymentEndpoint.write(value.`payableEndpoints`, buf)
    }
}




public object FfiConverterTypePublicPaymentEndpointCandidate: FfiConverterRustBuffer<PublicPaymentEndpointCandidate> {
    override fun read(buf: ByteBuffer): PublicPaymentEndpointCandidate {
        return PublicPaymentEndpointCandidate(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PublicPaymentEndpointCandidate): ULong = (
            FfiConverterString.allocationSize(value.`candidateId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PublicPaymentEndpointCandidate, buf: ByteBuffer) {
        FfiConverterString.write(value.`candidateId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypePublicPaymentEndpointSelectionRequest: FfiConverterRustBuffer<PublicPaymentEndpointSelectionRequest> {
    override fun read(buf: ByteBuffer): PublicPaymentEndpointSelectionRequest {
        return PublicPaymentEndpointSelectionRequest(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalTypeFfiPaymentAmountContext.read(buf),
            FfiConverterSequenceTypePublicPaymentEndpointCandidate.read(buf),
        )
    }

    override fun allocationSize(value: PublicPaymentEndpointSelectionRequest): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterOptionalTypeFfiPaymentAmountContext.allocationSize(value.`amount`) +
            FfiConverterSequenceTypePublicPaymentEndpointCandidate.allocationSize(value.`candidates`)
    )

    override fun write(value: PublicPaymentEndpointSelectionRequest, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterOptionalTypeFfiPaymentAmountContext.write(value.`amount`, buf)
        FfiConverterSequenceTypePublicPaymentEndpointCandidate.write(value.`candidates`, buf)
    }
}




public object FfiConverterTypePublicReceivingDetail: FfiConverterRustBuffer<PublicReceivingDetail> {
    override fun read(buf: ByteBuffer): PublicReceivingDetail {
        return PublicReceivingDetail(
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
        )
    }

    override fun allocationSize(value: PublicReceivingDetail): ULong = (
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`)
    )

    override fun write(value: PublicReceivingDetail, buf: ByteBuffer) {
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
    }
}




public object FfiConverterTypeQueuedPrivateMessage: FfiConverterRustBuffer<QueuedPrivateMessage> {
    override fun read(buf: ByteBuffer): QueuedPrivateMessage {
        return QueuedPrivateMessage(
            FfiConverterULong.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypeOutboundPrivateMessageStatus.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiPrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: QueuedPrivateMessage): ULong = (
            FfiConverterULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`kind`) +
            FfiConverterTypeOutboundPrivateMessageStatus.allocationSize(value.`status`) +
            FfiConverterUInt.allocationSize(value.`attemptCount`) +
            FfiConverterString.allocationSize(value.`createdAt`) +
            FfiConverterString.allocationSize(value.`updatedAt`) +
            FfiConverterOptionalString.allocationSize(value.`lastAttemptAt`) +
            FfiConverterOptionalString.allocationSize(value.`sentAt`) +
            FfiConverterOptionalTypeFfiPrivateOperationError.allocationSize(value.`lastError`)
    )

    override fun write(value: QueuedPrivateMessage, buf: ByteBuffer) {
        FfiConverterULong.write(value.`outboundMessageId`, buf)
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`kind`, buf)
        FfiConverterTypeOutboundPrivateMessageStatus.write(value.`status`, buf)
        FfiConverterUInt.write(value.`attemptCount`, buf)
        FfiConverterString.write(value.`createdAt`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
        FfiConverterOptionalString.write(value.`lastAttemptAt`, buf)
        FfiConverterOptionalString.write(value.`sentAt`, buf)
        FfiConverterOptionalTypeFfiPrivateOperationError.write(value.`lastError`, buf)
    }
}




public object FfiConverterTypeReceiptAccessView: FfiConverterRustBuffer<ReceiptAccessView> {
    override fun read(buf: ByteBuffer): ReceiptAccessView {
        return ReceiptAccessView(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterTypeReceiptRetrievalStatus.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: ReceiptAccessView): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`eventId`) +
            FfiConverterString.allocationSize(value.`receiptId`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalString.allocationSize(value.`paymentRequestId`) +
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterTypeReceiptRetrievalStatus.allocationSize(value.`retrievalStatus`) +
            FfiConverterOptionalString.allocationSize(value.`retrievalAttemptedAt`) +
            FfiConverterOptionalString.allocationSize(value.`retrievedAt`) +
            FfiConverterString.allocationSize(value.`receivedAt`)
    )

    override fun write(value: ReceiptAccessView, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`eventId`, buf)
        FfiConverterString.write(value.`receiptId`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalString.write(value.`paymentRequestId`, buf)
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterTypeReceiptRetrievalStatus.write(value.`retrievalStatus`, buf)
        FfiConverterOptionalString.write(value.`retrievalAttemptedAt`, buf)
        FfiConverterOptionalString.write(value.`retrievedAt`, buf)
        FfiConverterString.write(value.`receivedAt`, buf)
    }
}




public object FfiConverterTypeReceiptAmount: FfiConverterRustBuffer<ReceiptAmount> {
    override fun read(buf: ByteBuffer): ReceiptAmount {
        return ReceiptAmount(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: ReceiptAmount): ULong = (
            FfiConverterString.allocationSize(value.`value`) +
            FfiConverterString.allocationSize(value.`asset`)
    )

    override fun write(value: ReceiptAmount, buf: ByteBuffer) {
        FfiConverterString.write(value.`value`, buf)
        FfiConverterString.write(value.`asset`, buf)
    }
}




public object FfiConverterTypeReceiptDraft: FfiConverterRustBuffer<ReceiptDraft> {
    override fun read(buf: ByteBuffer): ReceiptDraft {
        return ReceiptDraft(
            FfiConverterOptionalString.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiReceiptAmount.read(buf),
            FfiConverterTypePrivateJsonObject.read(buf),
        )
    }

    override fun allocationSize(value: ReceiptDraft): ULong = (
            FfiConverterOptionalString.allocationSize(value.`receiptId`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalString.allocationSize(value.`paymentRequestId`) +
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterOptionalString.allocationSize(value.`paymentEndpointIdentifier`) +
            FfiConverterOptionalTypeFfiReceiptAmount.allocationSize(value.`amount`) +
            FfiConverterTypePrivateJsonObject.allocationSize(value.`metadata`)
    )

    override fun write(value: ReceiptDraft, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`receiptId`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalString.write(value.`paymentRequestId`, buf)
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterOptionalString.write(value.`paymentEndpointIdentifier`, buf)
        FfiConverterOptionalTypeFfiReceiptAmount.write(value.`amount`, buf)
        FfiConverterTypePrivateJsonObject.write(value.`metadata`, buf)
    }
}




public object FfiConverterTypeReceiptIssuanceView: FfiConverterRustBuffer<ReceiptIssuanceView> {
    override fun read(buf: ByteBuffer): ReceiptIssuanceView {
        return ReceiptIssuanceView(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiReceiptAmount.read(buf),
            FfiConverterTypeReceiptIssuanceStatus.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
        )
    }

    override fun allocationSize(value: ReceiptIssuanceView): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`receiptId`) +
            FfiConverterString.allocationSize(value.`receiptAccessEventId`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalString.allocationSize(value.`paymentRequestId`) +
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterOptionalString.allocationSize(value.`paymentEndpointIdentifier`) +
            FfiConverterOptionalTypeFfiReceiptAmount.allocationSize(value.`amount`) +
            FfiConverterTypeReceiptIssuanceStatus.allocationSize(value.`status`) +
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterString.allocationSize(value.`createdAt`) +
            FfiConverterString.allocationSize(value.`updatedAt`) +
            FfiConverterOptionalString.allocationSize(value.`storedAt`) +
            FfiConverterOptionalString.allocationSize(value.`accessQueuedAt`)
    )

    override fun write(value: ReceiptIssuanceView, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`receiptId`, buf)
        FfiConverterString.write(value.`receiptAccessEventId`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalString.write(value.`paymentRequestId`, buf)
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterOptionalString.write(value.`paymentEndpointIdentifier`, buf)
        FfiConverterOptionalTypeFfiReceiptAmount.write(value.`amount`, buf)
        FfiConverterTypeReceiptIssuanceStatus.write(value.`status`, buf)
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterString.write(value.`createdAt`, buf)
        FfiConverterString.write(value.`updatedAt`, buf)
        FfiConverterOptionalString.write(value.`storedAt`, buf)
        FfiConverterOptionalString.write(value.`accessQueuedAt`, buf)
    }
}




public object FfiConverterTypeReceiptRecord: FfiConverterRustBuffer<ReceiptRecord> {
    override fun read(buf: ByteBuffer): ReceiptRecord {
        return ReceiptRecord(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentReference.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiBillingPeriod.read(buf),
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalTypeFfiReceiptAmount.read(buf),
            FfiConverterTypePrivateJsonObject.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: ReceiptRecord): ULong = (
            FfiConverterString.allocationSize(value.`issuer`) +
            FfiConverterString.allocationSize(value.`issuerReceiverPath`) +
            FfiConverterString.allocationSize(value.`receiptAccessEventId`) +
            FfiConverterString.allocationSize(value.`receiptId`) +
            FfiConverterTypePaymentReference.allocationSize(value.`paymentReference`) +
            FfiConverterOptionalString.allocationSize(value.`paymentRequestId`) +
            FfiConverterOptionalTypeFfiBillingPeriod.allocationSize(value.`billingPeriod`) +
            FfiConverterString.allocationSize(value.`recipientPublicKey`) +
            FfiConverterOptionalString.allocationSize(value.`paymentEndpointIdentifier`) +
            FfiConverterOptionalTypeFfiReceiptAmount.allocationSize(value.`amount`) +
            FfiConverterTypePrivateJsonObject.allocationSize(value.`metadata`) +
            FfiConverterString.allocationSize(value.`retrievedAt`)
    )

    override fun write(value: ReceiptRecord, buf: ByteBuffer) {
        FfiConverterString.write(value.`issuer`, buf)
        FfiConverterString.write(value.`issuerReceiverPath`, buf)
        FfiConverterString.write(value.`receiptAccessEventId`, buf)
        FfiConverterString.write(value.`receiptId`, buf)
        FfiConverterTypePaymentReference.write(value.`paymentReference`, buf)
        FfiConverterOptionalString.write(value.`paymentRequestId`, buf)
        FfiConverterOptionalTypeFfiBillingPeriod.write(value.`billingPeriod`, buf)
        FfiConverterString.write(value.`recipientPublicKey`, buf)
        FfiConverterOptionalString.write(value.`paymentEndpointIdentifier`, buf)
        FfiConverterOptionalTypeFfiReceiptAmount.write(value.`amount`, buf)
        FfiConverterTypePrivateJsonObject.write(value.`metadata`, buf)
        FfiConverterString.write(value.`retrievedAt`, buf)
    }
}




public object FfiConverterTypeRecoveryMarkerPublishFailure: FfiConverterRustBuffer<RecoveryMarkerPublishFailure> {
    override fun read(buf: ByteBuffer): RecoveryMarkerPublishFailure {
        return RecoveryMarkerPublishFailure(
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypePrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: RecoveryMarkerPublishFailure): ULong = (
            FfiConverterOptionalULong.allocationSize(value.`outboundMessageId`) +
            FfiConverterTypePrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: RecoveryMarkerPublishFailure, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`outboundMessageId`, buf)
        FfiConverterTypePrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeReservationCleanupFailure: FfiConverterRustBuffer<ReservationCleanupFailure> {
    override fun read(buf: ByteBuffer): ReservationCleanupFailure {
        return ReservationCleanupFailure(
            FfiConverterOptionalString.read(buf),
            FfiConverterTypePrivateOperationError.read(buf),
        )
    }

    override fun allocationSize(value: ReservationCleanupFailure): ULong = (
            FfiConverterOptionalString.allocationSize(value.`reservationId`) +
            FfiConverterTypePrivateOperationError.allocationSize(value.`error`)
    )

    override fun write(value: ReservationCleanupFailure, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`reservationId`, buf)
        FfiConverterTypePrivateOperationError.write(value.`error`, buf)
    }
}




public object FfiConverterTypeResolvedPrivatePaymentEndpoint: FfiConverterRustBuffer<ResolvedPrivatePaymentEndpoint> {
    override fun read(buf: ByteBuffer): ResolvedPrivatePaymentEndpoint {
        return ResolvedPrivatePaymentEndpoint(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
            FfiConverterTypePaymentTarget.read(buf),
        )
    }

    override fun allocationSize(value: ResolvedPrivatePaymentEndpoint): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`) +
            FfiConverterTypePaymentTarget.allocationSize(value.`target`)
    )

    override fun write(value: ResolvedPrivatePaymentEndpoint, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
        FfiConverterTypePaymentTarget.write(value.`target`, buf)
    }
}




public object FfiConverterTypeResolvedPublicPaymentEndpoint: FfiConverterRustBuffer<ResolvedPublicPaymentEndpoint> {
    override fun read(buf: ByteBuffer): ResolvedPublicPaymentEndpoint {
        return ResolvedPublicPaymentEndpoint(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterTypePaymentPayload.read(buf),
            FfiConverterTypePaymentTarget.read(buf),
        )
    }

    override fun allocationSize(value: ResolvedPublicPaymentEndpoint): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`) +
            FfiConverterString.allocationSize(value.`identifier`) +
            FfiConverterTypePaymentPayload.allocationSize(value.`payload`) +
            FfiConverterTypePaymentTarget.allocationSize(value.`target`)
    )

    override fun write(value: ResolvedPublicPaymentEndpoint, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
        FfiConverterString.write(value.`identifier`, buf)
        FfiConverterTypePaymentPayload.write(value.`payload`, buf)
        FfiConverterTypePaymentTarget.write(value.`target`, buf)
    }
}




public object FfiConverterTypeRestoreRecoveryRequiredPeer: FfiConverterRustBuffer<RestoreRecoveryRequiredPeer> {
    override fun read(buf: ByteBuffer): RestoreRecoveryRequiredPeer {
        return RestoreRecoveryRequiredPeer(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: RestoreRecoveryRequiredPeer): ULong = (
            FfiConverterString.allocationSize(value.`counterparty`) +
            FfiConverterString.allocationSize(value.`counterpartyReceiverPath`)
    )

    override fun write(value: RestoreRecoveryRequiredPeer, buf: ByteBuffer) {
        FfiConverterString.write(value.`counterparty`, buf)
        FfiConverterString.write(value.`counterpartyReceiverPath`, buf)
    }
}




public object FfiConverterTypeRestoreReport: FfiConverterRustBuffer<RestoreReport> {
    override fun read(buf: ByteBuffer): RestoreReport {
        return RestoreReport(
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
            FfiConverterSequenceTypeRestoreRecoveryRequiredPeer.read(buf),
        )
    }

    override fun allocationSize(value: RestoreReport): ULong = (
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
            FfiConverterSequenceTypeRestoreRecoveryRequiredPeer.allocationSize(value.`recoveryRequiredPeers`)
    )

    override fun write(value: RestoreReport, buf: ByteBuffer) {
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
        FfiConverterSequenceTypeRestoreRecoveryRequiredPeer.write(value.`recoveryRequiredPeers`, buf)
    }
}




public object FfiConverterTypeSdkStateBlobSnapshot: FfiConverterRustBuffer<SdkStateBlobSnapshot> {
    override fun read(buf: ByteBuffer): SdkStateBlobSnapshot {
        return SdkStateBlobSnapshot(
            FfiConverterTypeSdkStateBlob.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: SdkStateBlobSnapshot): ULong = (
            FfiConverterTypeSdkStateBlob.allocationSize(value.`blob`) +
            FfiConverterString.allocationSize(value.`revision`)
    )

    override fun write(value: SdkStateBlobSnapshot, buf: ByteBuffer) {
        FfiConverterTypeSdkStateBlob.write(value.`blob`, buf)
        FfiConverterString.write(value.`revision`, buf)
    }
}





public object FfiConverterTypeAllowanceHistoryStatus: FfiConverterRustBuffer<AllowanceHistoryStatus> {
    override fun read(buf: ByteBuffer): AllowanceHistoryStatus = try {
        AllowanceHistoryStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: AllowanceHistoryStatus): ULong = 4UL

    override fun write(value: AllowanceHistoryStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeAllowanceLifecycleState: FfiConverterRustBuffer<AllowanceLifecycleState> {
    override fun read(buf: ByteBuffer): AllowanceLifecycleState = try {
        AllowanceLifecycleState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: AllowanceLifecycleState): ULong = 4UL

    override fun write(value: AllowanceLifecycleState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeAllowanceLocalRole: FfiConverterRustBuffer<AllowanceLocalRole> {
    override fun read(buf: ByteBuffer): AllowanceLocalRole = try {
        AllowanceLocalRole.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: AllowanceLocalRole): ULong = 4UL

    override fun write(value: AllowanceLocalRole, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeContactProfileSource: FfiConverterRustBuffer<ContactProfileSource> {
    override fun read(buf: ByteBuffer): ContactProfileSource = try {
        ContactProfileSource.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: ContactProfileSource): ULong = 4UL

    override fun write(value: ContactProfileSource, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeEncryptedLinkHandshakeRole: FfiConverterRustBuffer<EncryptedLinkHandshakeRole> {
    override fun read(buf: ByteBuffer): EncryptedLinkHandshakeRole = try {
        EncryptedLinkHandshakeRole.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: EncryptedLinkHandshakeRole): ULong = 4UL

    override fun write(value: EncryptedLinkHandshakeRole, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeEncryptedLinkRecoveryMarkerPolicy: FfiConverterRustBuffer<EncryptedLinkRecoveryMarkerPolicy> {
    override fun read(buf: ByteBuffer): EncryptedLinkRecoveryMarkerPolicy = try {
        EncryptedLinkRecoveryMarkerPolicy.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: EncryptedLinkRecoveryMarkerPolicy): ULong = 4UL

    override fun write(value: EncryptedLinkRecoveryMarkerPolicy, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeEndpointManagementScope: FfiConverterRustBuffer<EndpointManagementScope> {
    override fun read(buf: ByteBuffer): EndpointManagementScope = try {
        EndpointManagementScope.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: EndpointManagementScope): ULong = 4UL

    override fun write(value: EndpointManagementScope, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeLinkedPeerState: FfiConverterRustBuffer<LinkedPeerState> {
    override fun read(buf: ByteBuffer): LinkedPeerState = try {
        LinkedPeerState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: LinkedPeerState): ULong = 4UL

    override fun write(value: LinkedPeerState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeOutboundPrivateMessageStatus: FfiConverterRustBuffer<OutboundPrivateMessageStatus> {
    override fun read(buf: ByteBuffer): OutboundPrivateMessageStatus = try {
        OutboundPrivateMessageStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: OutboundPrivateMessageStatus): ULong = 4UL

    override fun write(value: OutboundPrivateMessageStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePaymentRequestLifecycleState: FfiConverterRustBuffer<PaymentRequestLifecycleState> {
    override fun read(buf: ByteBuffer): PaymentRequestLifecycleState = try {
        PaymentRequestLifecycleState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PaymentRequestLifecycleState): ULong = 4UL

    override fun write(value: PaymentRequestLifecycleState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePaymentRequestLocalRole: FfiConverterRustBuffer<PaymentRequestLocalRole> {
    override fun read(buf: ByteBuffer): PaymentRequestLocalRole = try {
        PaymentRequestLocalRole.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PaymentRequestLocalRole): ULong = 4UL

    override fun write(value: PaymentRequestLocalRole, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePrivatePaymentResolutionState: FfiConverterRustBuffer<PrivatePaymentResolutionState> {
    override fun read(buf: ByteBuffer): PrivatePaymentResolutionState = try {
        PrivatePaymentResolutionState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PrivatePaymentResolutionState): ULong = 4UL

    override fun write(value: PrivatePaymentResolutionState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePrivatePaymentResolutionStatus: FfiConverterRustBuffer<PrivatePaymentResolutionStatus> {
    override fun read(buf: ByteBuffer): PrivatePaymentResolutionStatus = try {
        PrivatePaymentResolutionStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PrivatePaymentResolutionStatus): ULong = 4UL

    override fun write(value: PrivatePaymentResolutionStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePrivateReceivingDetailReservationResponseKind: FfiConverterRustBuffer<PrivateReceivingDetailReservationResponseKind> {
    override fun read(buf: ByteBuffer): PrivateReceivingDetailReservationResponseKind = try {
        PrivateReceivingDetailReservationResponseKind.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PrivateReceivingDetailReservationResponseKind): ULong = 4UL

    override fun write(value: PrivateReceivingDetailReservationResponseKind, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




public object PubkyAuthCompanionClaimApprovalExceptionErrorHandler : UniffiRustCallStatusErrorHandler<PubkyAuthCompanionClaimApprovalException> {
    override fun lift(errorBuf: RustBufferByValue): PubkyAuthCompanionClaimApprovalException = FfiConverterTypePubkyAuthCompanionClaimApprovalError.lift(errorBuf)
}

public object FfiConverterTypePubkyAuthCompanionClaimApprovalError : FfiConverterRustBuffer<PubkyAuthCompanionClaimApprovalException> {
    override fun read(buf: ByteBuffer): PubkyAuthCompanionClaimApprovalException {
        return when (buf.getInt()) {
            1 -> PubkyAuthCompanionClaimApprovalException.InvalidAuthUrl(
                FfiConverterString.read(buf),
                )
            2 -> PubkyAuthCompanionClaimApprovalException.InvalidClaim(
                FfiConverterString.read(buf),
                )
            3 -> PubkyAuthCompanionClaimApprovalException.InvalidLocalSecretKey(
                FfiConverterString.read(buf),
                )
            4 -> PubkyAuthCompanionClaimApprovalException.EncryptionFailure(
                FfiConverterString.read(buf),
                )
            5 -> PubkyAuthCompanionClaimApprovalException.RelayDeliveryFailure(
                FfiConverterString.read(buf),
                )
            6 -> PubkyAuthCompanionClaimApprovalException.AuthorizationFailure(
                FfiConverterString.read(buf),
                )
            7 -> PubkyAuthCompanionClaimApprovalException.Unexpected(
                FfiConverterString.read(buf),
                )
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PubkyAuthCompanionClaimApprovalException): ULong {
        return when (value) {
            is PubkyAuthCompanionClaimApprovalException.InvalidAuthUrl -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.InvalidClaim -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.InvalidLocalSecretKey -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.EncryptionFailure -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.RelayDeliveryFailure -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.AuthorizationFailure -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PubkyAuthCompanionClaimApprovalException.Unexpected -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
        }
    }

    override fun write(value: PubkyAuthCompanionClaimApprovalException, buf: ByteBuffer) {
        when (value) {
            is PubkyAuthCompanionClaimApprovalException.InvalidAuthUrl -> {
                buf.putInt(1)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.InvalidClaim -> {
                buf.putInt(2)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.InvalidLocalSecretKey -> {
                buf.putInt(3)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.EncryptionFailure -> {
                buf.putInt(4)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.RelayDeliveryFailure -> {
                buf.putInt(5)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.AuthorizationFailure -> {
                buf.putInt(6)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PubkyAuthCompanionClaimApprovalException.Unexpected -> {
                buf.putInt(7)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





public object FfiConverterTypePubkyAuthRequestKind: FfiConverterRustBuffer<PubkyAuthRequestKind> {
    override fun read(buf: ByteBuffer): PubkyAuthRequestKind = try {
        PubkyAuthRequestKind.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PubkyAuthRequestKind): ULong = 4UL

    override fun write(value: PubkyAuthRequestKind, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePublicContactSharingPolicy: FfiConverterRustBuffer<PublicContactSharingPolicy> {
    override fun read(buf: ByteBuffer): PublicContactSharingPolicy = try {
        PublicContactSharingPolicy.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PublicContactSharingPolicy): ULong = 4UL

    override fun write(value: PublicContactSharingPolicy, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePublicPaymentResolutionStatus: FfiConverterRustBuffer<PublicPaymentResolutionStatus> {
    override fun read(buf: ByteBuffer): PublicPaymentResolutionStatus = try {
        PublicPaymentResolutionStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PublicPaymentResolutionStatus): ULong = 4UL

    override fun write(value: PublicPaymentResolutionStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypePublicationStatus: FfiConverterRustBuffer<PublicationStatus> {
    override fun read(buf: ByteBuffer): PublicationStatus = try {
        PublicationStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PublicationStatus): ULong = 4UL

    override fun write(value: PublicationStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeReceiptIssuanceStatus: FfiConverterRustBuffer<ReceiptIssuanceStatus> {
    override fun read(buf: ByteBuffer): ReceiptIssuanceStatus = try {
        ReceiptIssuanceStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: ReceiptIssuanceStatus): ULong = 4UL

    override fun write(value: ReceiptIssuanceStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





public object FfiConverterTypeReceiptRetrievalStatus: FfiConverterRustBuffer<ReceiptRetrievalStatus> {
    override fun read(buf: ByteBuffer): ReceiptRetrievalStatus = try {
        ReceiptRetrievalStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: ReceiptRetrievalStatus): ULong = 4UL

    override fun write(value: ReceiptRetrievalStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




public object PaykitExceptionErrorHandler : UniffiRustCallStatusErrorHandler<PaykitException> {
    override fun lift(errorBuf: RustBufferByValue): PaykitException = FfiConverterTypePaykitError.lift(errorBuf)
}

public object FfiConverterTypePaykitError : FfiConverterRustBuffer<PaykitException> {
    override fun read(buf: ByteBuffer): PaykitException {
        return when (buf.getInt()) {
            1 -> PaykitException.Storage(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            2 -> PaykitException.Identity(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            3 -> PaykitException.Transport(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            4 -> PaykitException.NotFound(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            5 -> PaykitException.Protocol(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            6 -> PaykitException.Policy(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            7 -> PaykitException.PaymentAdapter(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            8 -> PaykitException.RecoveryRequired(
                FfiConverterString.read(buf),
                FfiConverterString.read(buf),
                )
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PaykitException): ULong {
        return when (value) {
            is PaykitException.Storage -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.Identity -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.Transport -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.NotFound -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.Protocol -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.Policy -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.PaymentAdapter -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
            is PaykitException.RecoveryRequired -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`code`)
                + FfiConverterString.allocationSize(value.`context`)
            )
        }
    }

    override fun write(value: PaykitException, buf: ByteBuffer) {
        when (value) {
            is PaykitException.Storage -> {
                buf.putInt(1)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.Identity -> {
                buf.putInt(2)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.Transport -> {
                buf.putInt(3)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.NotFound -> {
                buf.putInt(4)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.Protocol -> {
                buf.putInt(5)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.Policy -> {
                buf.putInt(6)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.PaymentAdapter -> {
                buf.putInt(7)
                FfiConverterString.write(value.`code`, buf)
                FfiConverterString.write(value.`context`, buf)
                Unit
            }
            is PaykitException.RecoveryRequired -> {
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




public object FfiConverterOptionalBoolean: FfiConverterRustBuffer<kotlin.Boolean?> {
    override fun read(buf: ByteBuffer): kotlin.Boolean? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterBoolean.read(buf)
    }

    override fun allocationSize(value: kotlin.Boolean?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterBoolean.allocationSize(value)
        }
    }

    override fun write(value: kotlin.Boolean?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterBoolean.write(value, buf)
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




public object FfiConverterOptionalTypeFfiAllowanceTerms: FfiConverterRustBuffer<AllowanceTerms?> {
    override fun read(buf: ByteBuffer): AllowanceTerms? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAllowanceTerms.read(buf)
    }

    override fun allocationSize(value: AllowanceTerms?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAllowanceTerms.allocationSize(value)
        }
    }

    override fun write(value: AllowanceTerms?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAllowanceTerms.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivateOperationError: FfiConverterRustBuffer<PrivateOperationError?> {
    override fun read(buf: ByteBuffer): PrivateOperationError? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePrivateOperationError.read(buf)
    }

    override fun allocationSize(value: PrivateOperationError?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePrivateOperationError.allocationSize(value)
        }
    }

    override fun write(value: PrivateOperationError?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePrivateOperationError.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyLocalSecretKey: FfiConverterRustBuffer<PubkyLocalSecretKey?> {
    override fun read(buf: ByteBuffer): PubkyLocalSecretKey? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePubkyLocalSecretKey.read(buf)
    }

    override fun allocationSize(value: PubkyLocalSecretKey?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePubkyLocalSecretKey.allocationSize(value)
        }
    }

    override fun write(value: PubkyLocalSecretKey?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePubkyLocalSecretKey.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkySessionAccess: FfiConverterRustBuffer<PubkySessionAccess?> {
    override fun read(buf: ByteBuffer): PubkySessionAccess? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePubkySessionAccess.read(buf)
    }

    override fun allocationSize(value: PubkySessionAccess?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePubkySessionAccess.allocationSize(value)
        }
    }

    override fun write(value: PubkySessionAccess?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePubkySessionAccess.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiAllowanceAmountRange: FfiConverterRustBuffer<AllowanceAmountRange?> {
    override fun read(buf: ByteBuffer): AllowanceAmountRange? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAllowanceAmountRange.read(buf)
    }

    override fun allocationSize(value: AllowanceAmountRange?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAllowanceAmountRange.allocationSize(value)
        }
    }

    override fun write(value: AllowanceAmountRange?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAllowanceAmountRange.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiAllowanceRecord: FfiConverterRustBuffer<AllowanceRecord?> {
    override fun read(buf: ByteBuffer): AllowanceRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAllowanceRecord.read(buf)
    }

    override fun allocationSize(value: AllowanceRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAllowanceRecord.allocationSize(value)
        }
    }

    override fun write(value: AllowanceRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAllowanceRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiBillingPeriod: FfiConverterRustBuffer<BillingPeriod?> {
    override fun read(buf: ByteBuffer): BillingPeriod? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeBillingPeriod.read(buf)
    }

    override fun allocationSize(value: BillingPeriod?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeBillingPeriod.allocationSize(value)
        }
    }

    override fun write(value: BillingPeriod?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeBillingPeriod.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiContactProfileResolution: FfiConverterRustBuffer<ContactProfileResolution?> {
    override fun read(buf: ByteBuffer): ContactProfileResolution? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeContactProfileResolution.read(buf)
    }

    override fun allocationSize(value: ContactProfileResolution?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeContactProfileResolution.allocationSize(value)
        }
    }

    override fun write(value: ContactProfileResolution?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeContactProfileResolution.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiContactRecord: FfiConverterRustBuffer<ContactRecord?> {
    override fun read(buf: ByteBuffer): ContactRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeContactRecord.read(buf)
    }

    override fun allocationSize(value: ContactRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeContactRecord.allocationSize(value)
        }
    }

    override fun write(value: ContactRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeContactRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiEncryptedLinkRecoveryMarkerReport: FfiConverterRustBuffer<EncryptedLinkRecoveryMarkerReport?> {
    override fun read(buf: ByteBuffer): EncryptedLinkRecoveryMarkerReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeEncryptedLinkRecoveryMarkerReport.read(buf)
    }

    override fun allocationSize(value: EncryptedLinkRecoveryMarkerReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeEncryptedLinkRecoveryMarkerReport.allocationSize(value)
        }
    }

    override fun write(value: EncryptedLinkRecoveryMarkerReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeEncryptedLinkRecoveryMarkerReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiIdentityStatus: FfiConverterRustBuffer<IdentityStatus?> {
    override fun read(buf: ByteBuffer): IdentityStatus? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeIdentityStatus.read(buf)
    }

    override fun allocationSize(value: IdentityStatus?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeIdentityStatus.allocationSize(value)
        }
    }

    override fun write(value: IdentityStatus?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeIdentityStatus.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiLinkedPeerHandshakeReport: FfiConverterRustBuffer<LinkedPeerHandshakeReport?> {
    override fun read(buf: ByteBuffer): LinkedPeerHandshakeReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeLinkedPeerHandshakeReport.read(buf)
    }

    override fun allocationSize(value: LinkedPeerHandshakeReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeLinkedPeerHandshakeReport.allocationSize(value)
        }
    }

    override fun write(value: LinkedPeerHandshakeReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeLinkedPeerHandshakeReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiOutboundPrivateSendReport: FfiConverterRustBuffer<OutboundPrivateSendReport?> {
    override fun read(buf: ByteBuffer): OutboundPrivateSendReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeOutboundPrivateSendReport.read(buf)
    }

    override fun allocationSize(value: OutboundPrivateSendReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeOutboundPrivateSendReport.allocationSize(value)
        }
    }

    override fun write(value: OutboundPrivateSendReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeOutboundPrivateSendReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaykitProfile: FfiConverterRustBuffer<PaykitProfile?> {
    override fun read(buf: ByteBuffer): PaykitProfile? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaykitProfile.read(buf)
    }

    override fun allocationSize(value: PaykitProfile?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaykitProfile.allocationSize(value)
        }
    }

    override fun write(value: PaykitProfile?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaykitProfile.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaykitProfileRecord: FfiConverterRustBuffer<PaykitProfileRecord?> {
    override fun read(buf: ByteBuffer): PaykitProfileRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaykitProfileRecord.read(buf)
    }

    override fun allocationSize(value: PaykitProfileRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaykitProfileRecord.allocationSize(value)
        }
    }

    override fun write(value: PaykitProfileRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaykitProfileRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaykitReceiverMarker: FfiConverterRustBuffer<PaykitReceiverMarker?> {
    override fun read(buf: ByteBuffer): PaykitReceiverMarker? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaykitReceiverMarker.read(buf)
    }

    override fun allocationSize(value: PaykitReceiverMarker?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaykitReceiverMarker.allocationSize(value)
        }
    }

    override fun write(value: PaykitReceiverMarker?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaykitReceiverMarker.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaymentAmountContext: FfiConverterRustBuffer<PaymentAmountContext?> {
    override fun read(buf: ByteBuffer): PaymentAmountContext? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentAmountContext.read(buf)
    }

    override fun allocationSize(value: PaymentAmountContext?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentAmountContext.allocationSize(value)
        }
    }

    override fun write(value: PaymentAmountContext?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentAmountContext.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaymentRequestRecurrence: FfiConverterRustBuffer<PaymentRequestRecurrence?> {
    override fun read(buf: ByteBuffer): PaymentRequestRecurrence? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentRequestRecurrence.read(buf)
    }

    override fun allocationSize(value: PaymentRequestRecurrence?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentRequestRecurrence.allocationSize(value)
        }
    }

    override fun write(value: PaymentRequestRecurrence?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentRequestRecurrence.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaymentRequestTerms: FfiConverterRustBuffer<PaymentRequestTerms?> {
    override fun read(buf: ByteBuffer): PaymentRequestTerms? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentRequestTerms.read(buf)
    }

    override fun allocationSize(value: PaymentRequestTerms?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentRequestTerms.allocationSize(value)
        }
    }

    override fun write(value: PaymentRequestTerms?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentRequestTerms.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivatePaymentListView: FfiConverterRustBuffer<PrivatePaymentListView?> {
    override fun read(buf: ByteBuffer): PrivatePaymentListView? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePrivatePaymentListView.read(buf)
    }

    override fun allocationSize(value: PrivatePaymentListView?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePrivatePaymentListView.allocationSize(value)
        }
    }

    override fun write(value: PrivatePaymentListView?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePrivatePaymentListView.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPrivateStreamIntakeReport: FfiConverterRustBuffer<PrivateStreamIntakeReport?> {
    override fun read(buf: ByteBuffer): PrivateStreamIntakeReport? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePrivateStreamIntakeReport.read(buf)
    }

    override fun allocationSize(value: PrivateStreamIntakeReport?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePrivateStreamIntakeReport.allocationSize(value)
        }
    }

    override fun write(value: PrivateStreamIntakeReport?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePrivateStreamIntakeReport.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyProfile: FfiConverterRustBuffer<PubkyProfile?> {
    override fun read(buf: ByteBuffer): PubkyProfile? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePubkyProfile.read(buf)
    }

    override fun allocationSize(value: PubkyProfile?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePubkyProfile.allocationSize(value)
        }
    }

    override fun write(value: PubkyProfile?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePubkyProfile.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPubkyProfileRecord: FfiConverterRustBuffer<PubkyProfileRecord?> {
    override fun read(buf: ByteBuffer): PubkyProfileRecord? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePubkyProfileRecord.read(buf)
    }

    override fun allocationSize(value: PubkyProfileRecord?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePubkyProfileRecord.allocationSize(value)
        }
    }

    override fun write(value: PubkyProfileRecord?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePubkyProfileRecord.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiReceiptAmount: FfiConverterRustBuffer<ReceiptAmount?> {
    override fun read(buf: ByteBuffer): ReceiptAmount? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeReceiptAmount.read(buf)
    }

    override fun allocationSize(value: ReceiptAmount?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeReceiptAmount.allocationSize(value)
        }
    }

    override fun write(value: ReceiptAmount?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeReceiptAmount.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiSdkStateBlobSnapshot: FfiConverterRustBuffer<SdkStateBlobSnapshot?> {
    override fun read(buf: ByteBuffer): SdkStateBlobSnapshot? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeSdkStateBlobSnapshot.read(buf)
    }

    override fun allocationSize(value: SdkStateBlobSnapshot?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeSdkStateBlobSnapshot.allocationSize(value)
        }
    }

    override fun write(value: SdkStateBlobSnapshot?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeSdkStateBlobSnapshot.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiAllowanceLocalRole: FfiConverterRustBuffer<AllowanceLocalRole?> {
    override fun read(buf: ByteBuffer): AllowanceLocalRole? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAllowanceLocalRole.read(buf)
    }

    override fun allocationSize(value: AllowanceLocalRole?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAllowanceLocalRole.allocationSize(value)
        }
    }

    override fun write(value: AllowanceLocalRole?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAllowanceLocalRole.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiEncryptedLinkHandshakeRole: FfiConverterRustBuffer<EncryptedLinkHandshakeRole?> {
    override fun read(buf: ByteBuffer): EncryptedLinkHandshakeRole? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeEncryptedLinkHandshakeRole.read(buf)
    }

    override fun allocationSize(value: EncryptedLinkHandshakeRole?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeEncryptedLinkHandshakeRole.allocationSize(value)
        }
    }

    override fun write(value: EncryptedLinkHandshakeRole?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeEncryptedLinkHandshakeRole.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiOutboundPrivateMessageStatus: FfiConverterRustBuffer<OutboundPrivateMessageStatus?> {
    override fun read(buf: ByteBuffer): OutboundPrivateMessageStatus? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeOutboundPrivateMessageStatus.read(buf)
    }

    override fun allocationSize(value: OutboundPrivateMessageStatus?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeOutboundPrivateMessageStatus.allocationSize(value)
        }
    }

    override fun write(value: OutboundPrivateMessageStatus?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeOutboundPrivateMessageStatus.write(value, buf)
        }
    }
}




public object FfiConverterOptionalTypeFfiPaymentRequestLocalRole: FfiConverterRustBuffer<PaymentRequestLocalRole?> {
    override fun read(buf: ByteBuffer): PaymentRequestLocalRole? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentRequestLocalRole.read(buf)
    }

    override fun allocationSize(value: PaymentRequestLocalRole?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentRequestLocalRole.allocationSize(value)
        }
    }

    override fun write(value: PaymentRequestLocalRole?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentRequestLocalRole.write(value, buf)
        }
    }
}




public object FfiConverterOptionalSequenceString: FfiConverterRustBuffer<List<kotlin.String>?> {
    override fun read(buf: ByteBuffer): List<kotlin.String>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceString.read(buf)
    }

    override fun allocationSize(value: List<kotlin.String>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceString.allocationSize(value)
        }
    }

    override fun write(value: List<kotlin.String>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceString.write(value, buf)
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




public object FfiConverterSequenceTypeAllowancePeriodLimit: FfiConverterRustBuffer<List<AllowancePeriodLimit>> {
    override fun read(buf: ByteBuffer): List<AllowancePeriodLimit> {
        val len = buf.getInt()
        return List<AllowancePeriodLimit>(len) {
            FfiConverterTypeAllowancePeriodLimit.read(buf)
        }
    }

    override fun allocationSize(value: List<AllowancePeriodLimit>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeAllowancePeriodLimit.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<AllowancePeriodLimit>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeAllowancePeriodLimit.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeAllowanceRecord: FfiConverterRustBuffer<List<AllowanceRecord>> {
    override fun read(buf: ByteBuffer): List<AllowanceRecord> {
        val len = buf.getInt()
        return List<AllowanceRecord>(len) {
            FfiConverterTypeAllowanceRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<AllowanceRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeAllowanceRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<AllowanceRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeAllowanceRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeContactRecord: FfiConverterRustBuffer<List<ContactRecord>> {
    override fun read(buf: ByteBuffer): List<ContactRecord> {
        val len = buf.getInt()
        return List<ContactRecord>(len) {
            FfiConverterTypeContactRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<ContactRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeContactRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ContactRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeContactRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeCounterpartyReceiver: FfiConverterRustBuffer<List<CounterpartyReceiver>> {
    override fun read(buf: ByteBuffer): List<CounterpartyReceiver> {
        val len = buf.getInt()
        return List<CounterpartyReceiver>(len) {
            FfiConverterTypeCounterpartyReceiver.read(buf)
        }
    }

    override fun allocationSize(value: List<CounterpartyReceiver>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeCounterpartyReceiver.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<CounterpartyReceiver>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeCounterpartyReceiver.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeEndpointSyncChange: FfiConverterRustBuffer<List<EndpointSyncChange>> {
    override fun read(buf: ByteBuffer): List<EndpointSyncChange> {
        val len = buf.getInt()
        return List<EndpointSyncChange>(len) {
            FfiConverterTypeEndpointSyncChange.read(buf)
        }
    }

    override fun allocationSize(value: List<EndpointSyncChange>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeEndpointSyncChange.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<EndpointSyncChange>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeEndpointSyncChange.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeEventIdConflict: FfiConverterRustBuffer<List<EventIdConflict>> {
    override fun read(buf: ByteBuffer): List<EventIdConflict> {
        val len = buf.getInt()
        return List<EventIdConflict>(len) {
            FfiConverterTypeEventIdConflict.read(buf)
        }
    }

    override fun allocationSize(value: List<EventIdConflict>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeEventIdConflict.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<EventIdConflict>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeEventIdConflict.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeLinkedPeerRecord: FfiConverterRustBuffer<List<LinkedPeerRecord>> {
    override fun read(buf: ByteBuffer): List<LinkedPeerRecord> {
        val len = buf.getInt()
        return List<LinkedPeerRecord>(len) {
            FfiConverterTypeLinkedPeerRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<LinkedPeerRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeLinkedPeerRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<LinkedPeerRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeLinkedPeerRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeOutboundPrivateCounterpartySendReport: FfiConverterRustBuffer<List<OutboundPrivateCounterpartySendReport>> {
    override fun read(buf: ByteBuffer): List<OutboundPrivateCounterpartySendReport> {
        val len = buf.getInt()
        return List<OutboundPrivateCounterpartySendReport>(len) {
            FfiConverterTypeOutboundPrivateCounterpartySendReport.read(buf)
        }
    }

    override fun allocationSize(value: List<OutboundPrivateCounterpartySendReport>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeOutboundPrivateCounterpartySendReport.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<OutboundPrivateCounterpartySendReport>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeOutboundPrivateCounterpartySendReport.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeOutboundPrivateSendFailure: FfiConverterRustBuffer<List<OutboundPrivateSendFailure>> {
    override fun read(buf: ByteBuffer): List<OutboundPrivateSendFailure> {
        val len = buf.getInt()
        return List<OutboundPrivateSendFailure>(len) {
            FfiConverterTypeOutboundPrivateSendFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<OutboundPrivateSendFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeOutboundPrivateSendFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<OutboundPrivateSendFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeOutboundPrivateSendFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePaymentProofRecord: FfiConverterRustBuffer<List<PaymentProofRecord>> {
    override fun read(buf: ByteBuffer): List<PaymentProofRecord> {
        val len = buf.getInt()
        return List<PaymentProofRecord>(len) {
            FfiConverterTypePaymentProofRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<PaymentProofRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePaymentProofRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PaymentProofRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePaymentProofRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePaymentRequestRecord: FfiConverterRustBuffer<List<PaymentRequestRecord>> {
    override fun read(buf: ByteBuffer): List<PaymentRequestRecord> {
        val len = buf.getInt()
        return List<PaymentRequestRecord>(len) {
            FfiConverterTypePaymentRequestRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<PaymentRequestRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePaymentRequestRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PaymentRequestRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePaymentRequestRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentEndpointCandidate: FfiConverterRustBuffer<List<PrivatePaymentEndpointCandidate>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentEndpointCandidate> {
        val len = buf.getInt()
        return List<PrivatePaymentEndpointCandidate>(len) {
            FfiConverterTypePrivatePaymentEndpointCandidate.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentEndpointCandidate>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentEndpointCandidate.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentEndpointCandidate>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentEndpointCandidate.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentEndpointReservation: FfiConverterRustBuffer<List<PrivatePaymentEndpointReservation>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentEndpointReservation> {
        val len = buf.getInt()
        return List<PrivatePaymentEndpointReservation>(len) {
            FfiConverterTypePrivatePaymentEndpointReservation.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentEndpointReservation>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentEndpointReservation.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentEndpointReservation>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentEndpointReservation.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentEndpointReservationInput: FfiConverterRustBuffer<List<PrivatePaymentEndpointReservationInput>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentEndpointReservationInput> {
        val len = buf.getInt()
        return List<PrivatePaymentEndpointReservationInput>(len) {
            FfiConverterTypePrivatePaymentEndpointReservationInput.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentEndpointReservationInput>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentEndpointReservationInput.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentEndpointReservationInput>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentEndpointReservationInput.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentListDeliveryFailure: FfiConverterRustBuffer<List<PrivatePaymentListDeliveryFailure>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentListDeliveryFailure> {
        val len = buf.getInt()
        return List<PrivatePaymentListDeliveryFailure>(len) {
            FfiConverterTypePrivatePaymentListDeliveryFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentListDeliveryFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentListDeliveryFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentListDeliveryFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentListDeliveryFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentListEndpoint: FfiConverterRustBuffer<List<PrivatePaymentListEndpoint>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentListEndpoint> {
        val len = buf.getInt()
        return List<PrivatePaymentListEndpoint>(len) {
            FfiConverterTypePrivatePaymentListEndpoint.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentListEndpoint>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentListEndpoint.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentListEndpoint>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentListEndpoint.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentListReservationUpdateInput: FfiConverterRustBuffer<List<PrivatePaymentListReservationUpdateInput>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentListReservationUpdateInput> {
        val len = buf.getInt()
        return List<PrivatePaymentListReservationUpdateInput>(len) {
            FfiConverterTypePrivatePaymentListReservationUpdateInput.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentListReservationUpdateInput>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentListReservationUpdateInput.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentListReservationUpdateInput>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentListReservationUpdateInput.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivatePaymentListSyncChange: FfiConverterRustBuffer<List<PrivatePaymentListSyncChange>> {
    override fun read(buf: ByteBuffer): List<PrivatePaymentListSyncChange> {
        val len = buf.getInt()
        return List<PrivatePaymentListSyncChange>(len) {
            FfiConverterTypePrivatePaymentListSyncChange.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivatePaymentListSyncChange>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivatePaymentListSyncChange.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivatePaymentListSyncChange>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivatePaymentListSyncChange.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivateReceivingDetail: FfiConverterRustBuffer<List<PrivateReceivingDetail>> {
    override fun read(buf: ByteBuffer): List<PrivateReceivingDetail> {
        val len = buf.getInt()
        return List<PrivateReceivingDetail>(len) {
            FfiConverterTypePrivateReceivingDetail.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivateReceivingDetail>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivateReceivingDetail.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivateReceivingDetail>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivateReceivingDetail.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePrivateStreamCounterpartyIntakeReport: FfiConverterRustBuffer<List<PrivateStreamCounterpartyIntakeReport>> {
    override fun read(buf: ByteBuffer): List<PrivateStreamCounterpartyIntakeReport> {
        val len = buf.getInt()
        return List<PrivateStreamCounterpartyIntakeReport>(len) {
            FfiConverterTypePrivateStreamCounterpartyIntakeReport.read(buf)
        }
    }

    override fun allocationSize(value: List<PrivateStreamCounterpartyIntakeReport>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePrivateStreamCounterpartyIntakeReport.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PrivateStreamCounterpartyIntakeReport>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePrivateStreamCounterpartyIntakeReport.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePubkyProfileLink: FfiConverterRustBuffer<List<PubkyProfileLink>> {
    override fun read(buf: ByteBuffer): List<PubkyProfileLink> {
        val len = buf.getInt()
        return List<PubkyProfileLink>(len) {
            FfiConverterTypePubkyProfileLink.read(buf)
        }
    }

    override fun allocationSize(value: List<PubkyProfileLink>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePubkyProfileLink.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PubkyProfileLink>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePubkyProfileLink.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePublicPaymentEndpointCandidate: FfiConverterRustBuffer<List<PublicPaymentEndpointCandidate>> {
    override fun read(buf: ByteBuffer): List<PublicPaymentEndpointCandidate> {
        val len = buf.getInt()
        return List<PublicPaymentEndpointCandidate>(len) {
            FfiConverterTypePublicPaymentEndpointCandidate.read(buf)
        }
    }

    override fun allocationSize(value: List<PublicPaymentEndpointCandidate>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePublicPaymentEndpointCandidate.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PublicPaymentEndpointCandidate>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePublicPaymentEndpointCandidate.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePublicReceivingDetail: FfiConverterRustBuffer<List<PublicReceivingDetail>> {
    override fun read(buf: ByteBuffer): List<PublicReceivingDetail> {
        val len = buf.getInt()
        return List<PublicReceivingDetail>(len) {
            FfiConverterTypePublicReceivingDetail.read(buf)
        }
    }

    override fun allocationSize(value: List<PublicReceivingDetail>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePublicReceivingDetail.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PublicReceivingDetail>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePublicReceivingDetail.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeReceiptAccessView: FfiConverterRustBuffer<List<ReceiptAccessView>> {
    override fun read(buf: ByteBuffer): List<ReceiptAccessView> {
        val len = buf.getInt()
        return List<ReceiptAccessView>(len) {
            FfiConverterTypeReceiptAccessView.read(buf)
        }
    }

    override fun allocationSize(value: List<ReceiptAccessView>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeReceiptAccessView.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ReceiptAccessView>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeReceiptAccessView.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeReceiptIssuanceView: FfiConverterRustBuffer<List<ReceiptIssuanceView>> {
    override fun read(buf: ByteBuffer): List<ReceiptIssuanceView> {
        val len = buf.getInt()
        return List<ReceiptIssuanceView>(len) {
            FfiConverterTypeReceiptIssuanceView.read(buf)
        }
    }

    override fun allocationSize(value: List<ReceiptIssuanceView>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeReceiptIssuanceView.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ReceiptIssuanceView>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeReceiptIssuanceView.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeReceiptRecord: FfiConverterRustBuffer<List<ReceiptRecord>> {
    override fun read(buf: ByteBuffer): List<ReceiptRecord> {
        val len = buf.getInt()
        return List<ReceiptRecord>(len) {
            FfiConverterTypeReceiptRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<ReceiptRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeReceiptRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ReceiptRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeReceiptRecord.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeRecoveryMarkerPublishFailure: FfiConverterRustBuffer<List<RecoveryMarkerPublishFailure>> {
    override fun read(buf: ByteBuffer): List<RecoveryMarkerPublishFailure> {
        val len = buf.getInt()
        return List<RecoveryMarkerPublishFailure>(len) {
            FfiConverterTypeRecoveryMarkerPublishFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<RecoveryMarkerPublishFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeRecoveryMarkerPublishFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<RecoveryMarkerPublishFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeRecoveryMarkerPublishFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeReservationCleanupFailure: FfiConverterRustBuffer<List<ReservationCleanupFailure>> {
    override fun read(buf: ByteBuffer): List<ReservationCleanupFailure> {
        val len = buf.getInt()
        return List<ReservationCleanupFailure>(len) {
            FfiConverterTypeReservationCleanupFailure.read(buf)
        }
    }

    override fun allocationSize(value: List<ReservationCleanupFailure>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeReservationCleanupFailure.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ReservationCleanupFailure>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeReservationCleanupFailure.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeResolvedPrivatePaymentEndpoint: FfiConverterRustBuffer<List<ResolvedPrivatePaymentEndpoint>> {
    override fun read(buf: ByteBuffer): List<ResolvedPrivatePaymentEndpoint> {
        val len = buf.getInt()
        return List<ResolvedPrivatePaymentEndpoint>(len) {
            FfiConverterTypeResolvedPrivatePaymentEndpoint.read(buf)
        }
    }

    override fun allocationSize(value: List<ResolvedPrivatePaymentEndpoint>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeResolvedPrivatePaymentEndpoint.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ResolvedPrivatePaymentEndpoint>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeResolvedPrivatePaymentEndpoint.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeResolvedPublicPaymentEndpoint: FfiConverterRustBuffer<List<ResolvedPublicPaymentEndpoint>> {
    override fun read(buf: ByteBuffer): List<ResolvedPublicPaymentEndpoint> {
        val len = buf.getInt()
        return List<ResolvedPublicPaymentEndpoint>(len) {
            FfiConverterTypeResolvedPublicPaymentEndpoint.read(buf)
        }
    }

    override fun allocationSize(value: List<ResolvedPublicPaymentEndpoint>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeResolvedPublicPaymentEndpoint.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ResolvedPublicPaymentEndpoint>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeResolvedPublicPaymentEndpoint.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeRestoreRecoveryRequiredPeer: FfiConverterRustBuffer<List<RestoreRecoveryRequiredPeer>> {
    override fun read(buf: ByteBuffer): List<RestoreRecoveryRequiredPeer> {
        val len = buf.getInt()
        return List<RestoreRecoveryRequiredPeer>(len) {
            FfiConverterTypeRestoreRecoveryRequiredPeer.read(buf)
        }
    }

    override fun allocationSize(value: List<RestoreRecoveryRequiredPeer>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeRestoreRecoveryRequiredPeer.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<RestoreRecoveryRequiredPeer>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeRestoreRecoveryRequiredPeer.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypeAllowanceLifecycleState: FfiConverterRustBuffer<List<AllowanceLifecycleState>> {
    override fun read(buf: ByteBuffer): List<AllowanceLifecycleState> {
        val len = buf.getInt()
        return List<AllowanceLifecycleState>(len) {
            FfiConverterTypeAllowanceLifecycleState.read(buf)
        }
    }

    override fun allocationSize(value: List<AllowanceLifecycleState>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeAllowanceLifecycleState.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<AllowanceLifecycleState>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeAllowanceLifecycleState.write(it, buf)
        }
    }
}




public object FfiConverterSequenceTypePaymentRequestLifecycleState: FfiConverterRustBuffer<List<PaymentRequestLifecycleState>> {
    override fun read(buf: ByteBuffer): List<PaymentRequestLifecycleState> {
        val len = buf.getInt()
        return List<PaymentRequestLifecycleState>(len) {
            FfiConverterTypePaymentRequestLifecycleState.read(buf)
        }
    }

    override fun allocationSize(value: List<PaymentRequestLifecycleState>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePaymentRequestLifecycleState.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PaymentRequestLifecycleState>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePaymentRequestLifecycleState.write(it, buf)
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
 * Decode an SDK state blob snapshot previously encoded by Paykit FFI.
 */
@Throws(PaykitException::class)
public fun `decodeSdkStateBlobSnapshot`(`bytes`: kotlin.ByteArray): SdkStateBlobSnapshot {
    return FfiConverterTypeSdkStateBlobSnapshot.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_decode_sdk_state_blob_snapshot(
            FfiConverterByteArray.lower(`bytes`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the default SDK policy for an explicit Paykit receiver path.
 */
@Throws(PaykitException::class)
public fun `defaultConfig`(`receiverPath`: kotlin.String): PaykitSdkConfig {
    return FfiConverterTypePaykitSdkConfig.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_default_config(
            FfiConverterString.lower(`receiverPath`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the default Pubky client configuration.
 */
public fun `defaultPubkyClientConfig`(): PubkyClientConfig {
    return FfiConverterTypePubkyClientConfig.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_default_pubky_client_config(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Encode an SDK state blob snapshot for apps that store blob and revision together.
 */
@Throws(PaykitException::class)
public fun `encodeSdkStateBlobSnapshot`(`snapshot`: SdkStateBlobSnapshot): kotlin.ByteArray {
    return FfiConverterByteArray.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_encode_sdk_state_blob_snapshot(
            FfiConverterTypeSdkStateBlobSnapshot.lower(`snapshot`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Generate a fresh Receipt ID.
 */
public fun `generateReceiptId`(): kotlin.String {
    return FfiConverterString.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_generate_receipt_id(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Normalize raw z32 or `pubky...` public-key text to app-key form.
 */
@Throws(PaykitException::class)
public fun `normalizePubkyPublicKey`(`value`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_normalize_pubky_public_key(
            FfiConverterString.lower(`value`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Parse an auth deep link into public request details.
 */
@Throws(PaykitException::class)
public fun `parsePubkyAuthUrl`(`authUrl`: kotlin.String): PubkyAuthDetails {
    return FfiConverterTypePubkyAuthDetails.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_parse_pubky_auth_url(
            FfiConverterString.lower(`authUrl`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Parse a `pubky://<public-key>/<path>` resource into stable parts.
 */
@Throws(PaykitException::class)
public fun `parsePubkyResource`(`uri`: kotlin.String): PubkyResourceRef {
    return FfiConverterTypePubkyResourceRef.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_parse_pubky_resource(
            FfiConverterString.lower(`uri`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the Pubky public key for a local secret key.
 */
@Throws(PaykitException::class)
public fun `pubkyPublicKeyFromSecret`(`localSecretKey`: PubkyLocalSecretKey): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_pubky_public_key_from_secret(
            FfiConverterTypePubkyLocalSecretKey.lower(`localSecretKey`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Derive a local Pubky secret key from a BIP39 English mnemonic phrase.
 */
@Throws(PaykitException::class)
public fun `pubkySecretKeyFromBip39Mnemonic`(`mnemonicPhrase`: kotlin.String): PubkyLocalSecretKey {
    return FfiConverterTypePubkyLocalSecretKey.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_pubky_secret_key_from_bip39_mnemonic(
            FfiConverterString.lower(`mnemonicPhrase`),
            uniffiRustCallStatus,
        )
    }!!)
}

/**
 * Derive a local Pubky secret key from a 64-byte BIP39 seed.
 */
@Throws(PaykitException::class)
public fun `pubkySecretKeyFromBip39Seed`(`seed`: kotlin.ByteArray): PubkyLocalSecretKey {
    return FfiConverterTypePubkyLocalSecretKey.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_pubky_secret_key_from_bip39_seed(
            FfiConverterByteArray.lower(`seed`),
            uniffiRustCallStatus,
        )
    }!!)
}

/**
 * Normalize raw z32 or `pubky...` public-key text to raw z32 form.
 */
@Throws(PaykitException::class)
public fun `rawPubkyPublicKey`(`value`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_raw_pubky_public_key(
            FfiConverterString.lower(`value`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return a shortened `pubky...` public key for diagnostics.
 */
@Throws(PaykitException::class)
public fun `redactedPubkyPublicKey`(`value`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_redacted_pubky_public_key(
            FfiConverterString.lower(`value`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return Pubky capabilities required by this SDK configuration.
 */
@Throws(PaykitException::class)
public fun `requiredSessionCapabilities`(`config`: PaykitSdkConfig): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_required_session_capabilities(
            FfiConverterTypePaykitSdkConfig.lower(`config`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Resolve a Pubky URI into the transport URL used by Pubky storage.
 */
@Throws(PaykitException::class)
public fun `resolvePubkyUrl`(`uri`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitExceptionErrorHandler) { uniffiRustCallStatus ->
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
