

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
        if (uniffi_paykit_checksum_func_paykit_accept_encrypted_link() != 21287.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_advance_handshake() != 29494.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_close_encrypted_link() != 14508.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_default_max_recovery_attempts() != 23339.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_default_max_send_retries() != 12386.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_drop_encrypted_link_handshake() != 43355.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_encrypted_link_handshake_snapshot_recipient() != 33656.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_encrypted_link_snapshot_recipient() != 21528.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_export_session() != 8374.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_force_sign_out() != 30515.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_generate_payment_reference() != 10899.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_get_current_public_key() != 28037.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_get_payment_endpoint() != 52733.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_get_payment_list() != 63326.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_get_private_payments() != 50390.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_import_session() != 29532.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_initialize() != 62040.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_initiate_encrypted_link() != 52625.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_is_authenticated() != 34745.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_remove_payment_endpoint() != 52853.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_restore_encrypted_link() != 31079.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_restore_encrypted_link_handshake() != 23271.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_serialize_encrypted_link() != 33771.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_serialize_encrypted_link_handshake() != 27705.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_set_encrypted_link_handshake_max_recovery_attempts() != 38386.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_set_encrypted_link_max_send_retries() != 4305.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_set_payment_endpoint() != 62857.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_set_private_payments() != 52873.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_sign_in() != 50011.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_sign_out() != 116.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
        if (uniffi_paykit_checksum_func_paykit_sign_up() != 45538.toShort()) {
            throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
        }
    }

    // Integrity check functions only
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_accept_encrypted_link(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_advance_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_close_encrypted_link(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_default_max_recovery_attempts(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_default_max_send_retries(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_drop_encrypted_link_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_encrypted_link_handshake_snapshot_recipient(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_encrypted_link_snapshot_recipient(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_export_session(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_force_sign_out(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_generate_payment_reference(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_get_current_public_key(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_get_payment_endpoint(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_get_payment_list(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_get_private_payments(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_import_session(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_initialize(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_initiate_encrypted_link(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_is_authenticated(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_remove_payment_endpoint(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_restore_encrypted_link(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_restore_encrypted_link_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_serialize_encrypted_link(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_serialize_encrypted_link_handshake(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_set_encrypted_link_handshake_max_recovery_attempts(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_set_encrypted_link_max_send_retries(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_set_payment_endpoint(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_set_private_payments(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_sign_in(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_sign_out(
    ): Short
    @JvmStatic
    external fun uniffi_paykit_checksum_func_paykit_sign_up(
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
    }
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_accept_encrypted_link(
        `secretKeyHex`: RustBufferByValue,
        `senderPublicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_advance_handshake(
        `handshakeId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_close_encrypted_link(
        `linkId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_default_max_recovery_attempts(
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_default_max_send_retries(
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_drop_encrypted_link_handshake(
        `handshakeId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_encrypted_link_handshake_snapshot_recipient(
        `snapshotHex`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_encrypted_link_snapshot_recipient(
        `snapshotHex`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_export_session(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_force_sign_out(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_generate_payment_reference(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_get_current_public_key(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_get_payment_endpoint(
        `publicKey`: RustBufferByValue,
        `methodId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_get_payment_list(
        `publicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_get_private_payments(
        `linkId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_import_session(
        `sessionSecret`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_initialize(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_initiate_encrypted_link(
        `secretKeyHex`: RustBufferByValue,
        `receiverPublicKey`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_is_authenticated(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_remove_payment_endpoint(
        `methodId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_restore_encrypted_link(
        `secretKeyHex`: RustBufferByValue,
        `snapshotHex`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_restore_encrypted_link_handshake(
        `secretKeyHex`: RustBufferByValue,
        `snapshotHex`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_serialize_encrypted_link(
        `linkId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_serialize_encrypted_link_handshake(
        `handshakeId`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_set_encrypted_link_handshake_max_recovery_attempts(
        `handshakeId`: RustBufferByValue,
        `max`: Int,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_set_encrypted_link_max_send_retries(
        `linkId`: RustBufferByValue,
        `max`: Int,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_set_payment_endpoint(
        `methodId`: RustBufferByValue,
        `endpointData`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_set_private_payments(
        `linkId`: RustBufferByValue,
        `payload`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_sign_in(
        `secretKeyHex`: RustBufferByValue,
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_sign_out(
    ): Long
    @JvmStatic
    external fun uniffi_paykit_fn_func_paykit_sign_up(
        `secretKeyHex`: RustBufferByValue,
        `homeserverPublicKey`: RustBufferByValue,
    ): Long
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




public object FfiConverterTypeFfiHandshakeProgress: FfiConverterRustBuffer<FfiHandshakeProgress> {
    override fun read(buf: ByteBuffer): FfiHandshakeProgress {
        return FfiHandshakeProgress(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiHandshakeProgress): ULong = (
            FfiConverterString.allocationSize(value.`status`) +
            FfiConverterString.allocationSize(value.`handleId`)
    )

    override fun write(value: FfiHandshakeProgress, buf: ByteBuffer) {
        FfiConverterString.write(value.`status`, buf)
        FfiConverterString.write(value.`handleId`, buf)
    }
}




public object FfiConverterTypeFfiPaymentEntry: FfiConverterRustBuffer<FfiPaymentEntry> {
    override fun read(buf: ByteBuffer): FfiPaymentEntry {
        return FfiPaymentEntry(
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
        )
    }

    override fun allocationSize(value: FfiPaymentEntry): ULong = (
            FfiConverterString.allocationSize(value.`methodId`) +
            FfiConverterString.allocationSize(value.`endpointData`)
    )

    override fun write(value: FfiPaymentEntry, buf: ByteBuffer) {
        FfiConverterString.write(value.`methodId`, buf)
        FfiConverterString.write(value.`endpointData`, buf)
    }
}




public object FfiConverterTypeFfiPrivatePaymentsPayload: FfiConverterRustBuffer<FfiPrivatePaymentsPayload> {
    override fun read(buf: ByteBuffer): FfiPrivatePaymentsPayload {
        return FfiPrivatePaymentsPayload(
            FfiConverterString.read(buf),
            FfiConverterSequenceTypeFfiPaymentEntry.read(buf),
        )
    }

    override fun allocationSize(value: FfiPrivatePaymentsPayload): ULong = (
            FfiConverterString.allocationSize(value.`reference`) +
            FfiConverterSequenceTypeFfiPaymentEntry.allocationSize(value.`entries`)
    )

    override fun write(value: FfiPrivatePaymentsPayload, buf: ByteBuffer) {
        FfiConverterString.write(value.`reference`, buf)
        FfiConverterSequenceTypeFfiPaymentEntry.write(value.`entries`, buf)
    }
}




public object PaykitFfiExceptionErrorHandler : UniffiRustCallStatusErrorHandler<PaykitFfiException> {
    override fun lift(errorBuf: RustBufferByValue): PaykitFfiException = FfiConverterTypePaykitFfiError.lift(errorBuf)
}

public object FfiConverterTypePaykitFfiError : FfiConverterRustBuffer<PaykitFfiException> {
    override fun read(buf: ByteBuffer): PaykitFfiException {
        return when (buf.getInt()) {
            1 -> PaykitFfiException.Transport(
                FfiConverterString.read(buf),
                )
            2 -> PaykitFfiException.NotFound(
                FfiConverterString.read(buf),
                )
            3 -> PaykitFfiException.InvalidData(
                FfiConverterString.read(buf),
                )
            4 -> PaykitFfiException.Validation(
                FfiConverterString.read(buf),
                )
            5 -> PaykitFfiException.Session(
                FfiConverterString.read(buf),
                )
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PaykitFfiException): ULong {
        return when (value) {
            is PaykitFfiException.Transport -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PaykitFfiException.NotFound -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PaykitFfiException.InvalidData -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PaykitFfiException.Validation -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
            is PaykitFfiException.Session -> (
                // Add the size for the Int that specifies the variant plus the size needed for all fields
                4UL
                + FfiConverterString.allocationSize(value.`reason`)
            )
        }
    }

    override fun write(value: PaykitFfiException, buf: ByteBuffer) {
        when (value) {
            is PaykitFfiException.Transport -> {
                buf.putInt(1)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PaykitFfiException.NotFound -> {
                buf.putInt(2)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PaykitFfiException.InvalidData -> {
                buf.putInt(3)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PaykitFfiException.Validation -> {
                buf.putInt(4)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
            is PaykitFfiException.Session -> {
                buf.putInt(5)
                FfiConverterString.write(value.`reason`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
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




public object FfiConverterOptionalTypeFfiPrivatePaymentsPayload: FfiConverterRustBuffer<FfiPrivatePaymentsPayload?> {
    override fun read(buf: ByteBuffer): FfiPrivatePaymentsPayload? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFfiPrivatePaymentsPayload.read(buf)
    }

    override fun allocationSize(value: FfiPrivatePaymentsPayload?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFfiPrivatePaymentsPayload.allocationSize(value)
        }
    }

    override fun write(value: FfiPrivatePaymentsPayload?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFfiPrivatePaymentsPayload.write(value, buf)
        }
    }
}




public object FfiConverterSequenceTypeFfiPaymentEntry: FfiConverterRustBuffer<List<FfiPaymentEntry>> {
    override fun read(buf: ByteBuffer): List<FfiPaymentEntry> {
        val len = buf.getInt()
        return List<FfiPaymentEntry>(len) {
            FfiConverterTypeFfiPaymentEntry.read(buf)
        }
    }

    override fun allocationSize(value: List<FfiPaymentEntry>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeFfiPaymentEntry.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<FfiPaymentEntry>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeFfiPaymentEntry.write(it, buf)
        }
    }
}












/**
 * Start a private-payment encrypted link as the responder.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitAcceptEncryptedLink`(`secretKeyHex`: kotlin.String, `senderPublicKey`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_accept_encrypted_link(
            FfiConverterString.lower(`secretKeyHex`),
            FfiConverterString.lower(`senderPublicKey`),
        ),
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
 * Advance an encrypted-link handshake by one polling-safe step.
 *
 * Returns status `"pending"` with the same handshake handle, or `"complete"`
 * with a new encrypted-link handle.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitAdvanceHandshake`(`handshakeId`: kotlin.String): FfiHandshakeProgress {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_advance_handshake(
            FfiConverterString.lower(`handshakeId`),
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
        // lift function
        { FfiConverterTypeFfiHandshakeProgress.lift(it) },
        // Error FFI converter
        PaykitFfiExceptionErrorHandler,
    )
}

/**
 * Close an established encrypted link and remove its FFI handle.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitCloseEncryptedLink`(`linkId`: kotlin.String) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_close_encrypted_link(
            FfiConverterString.lower(`linkId`),
        ),
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
 * Default maximum number of consecutive handshake recovery attempts.
 */
public fun `paykitDefaultMaxRecoveryAttempts`(): kotlin.UInt {
    return FfiConverterUInt.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_paykit_default_max_recovery_attempts(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Default maximum number of automatic private-payment send retries.
 */
public fun `paykitDefaultMaxSendRetries`(): kotlin.UInt {
    return FfiConverterUInt.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_paykit_default_max_send_retries(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Drop an in-progress encrypted-link handshake handle.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitDropEncryptedLinkHandshake`(`handshakeId`: kotlin.String) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_drop_encrypted_link_handshake(
            FfiConverterString.lower(`handshakeId`),
        ),
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
 * Return the remote peer embedded in a handshake snapshot.
 */
@Throws(PaykitFfiException::class)
public fun `paykitEncryptedLinkHandshakeSnapshotRecipient`(`snapshotHex`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_paykit_encrypted_link_handshake_snapshot_recipient(
            FfiConverterString.lower(`snapshotHex`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Return the remote peer embedded in an encrypted-link snapshot.
 */
@Throws(PaykitFfiException::class)
public fun `paykitEncryptedLinkSnapshotRecipient`(`snapshotHex`: kotlin.String): kotlin.String {
    return FfiConverterString.lift(uniffiRustCallWithError(PaykitFfiExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_paykit_encrypted_link_snapshot_recipient(
            FfiConverterString.lower(`snapshotHex`),
            uniffiRustCallStatus,
        )
    })
}

/**
 * Exports the current session secret for persistence across app restarts.
 *
 * Returns the compact `<pubkey_z32>:<cookie_secret>` string that can be
 * passed back to `paykit_import_session` on next cold start.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitExportSession`(): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_export_session(
        ),
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
 * Discard the local session without contacting the homeserver.
 *
 * Idempotent — safe to call even when no session exists.
 * The server-side session will expire on its own.
 */
public suspend fun `paykitForceSignOut`() {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_force_sign_out(
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_void(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_void(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_void(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_void(future) },
        // lift function
        { Unit },
        
        // Error FFI converter
        UniffiNullRustCallStatusErrorHandler,
    )
}

/**
 * Generate a fresh UUID-v4 payment reference for private payment correlation.
 */
public fun `paykitGeneratePaymentReference`(): kotlin.String {
    return FfiConverterString.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.uniffi_paykit_fn_func_paykit_generate_payment_reference(
            uniffiRustCallStatus,
        )
    })
}

/**
 * Returns the public key of the currently authenticated user, or `None`.
 */
public suspend fun `paykitGetCurrentPublicKey`(): kotlin.String? {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_get_current_public_key(
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
        // lift function
        { FfiConverterOptionalString.lift(it) },
        // Error FFI converter
        UniffiNullRustCallStatusErrorHandler,
    )
}

/**
 * Fetch a single payment endpoint for a user and method. Returns `None` if not set.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitGetPaymentEndpoint`(`publicKey`: kotlin.String, `methodId`: kotlin.String): kotlin.String? {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_get_payment_endpoint(
            FfiConverterString.lower(`publicKey`),
            FfiConverterString.lower(`methodId`),
        ),
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
 * Fetch all published payment methods for a user.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitGetPaymentList`(`publicKey`: kotlin.String): List<FfiPaymentEntry> {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_get_payment_list(
            FfiConverterString.lower(`publicKey`),
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
        // lift function
        { FfiConverterSequenceTypeFfiPaymentEntry.lift(it) },
        // Error FFI converter
        PaykitFfiExceptionErrorHandler,
    )
}

/**
 * Receive and decrypt the latest private payments envelope from an established link.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitGetPrivatePayments`(`linkId`: kotlin.String): FfiPrivatePaymentsPayload? {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_get_private_payments(
            FfiConverterString.lower(`linkId`),
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_rust_buffer(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_rust_buffer(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_rust_buffer(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_rust_buffer(future) },
        // lift function
        { FfiConverterOptionalTypeFfiPrivatePaymentsPayload.lift(it) },
        // Error FFI converter
        PaykitFfiExceptionErrorHandler,
    )
}

/**
 * Import a session from a Pubky Ring auth flow.
 *
 * Accepts a compact session secret (`<pubkey_z32>:<cookie_secret>`) produced
 * by `PubkySession::export_secret()`. Validates with the homeserver and stores
 * the session for subsequent write operations.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitImportSession`(`sessionSecret`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_import_session(
            FfiConverterString.lower(`sessionSecret`),
        ),
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
 * Create the Pubky SDK facade and initialize logging. Call once at app startup.
 *
 * Targets the **production** network.
 *
 * Safe to call multiple times — subsequent calls are no-ops if the first
 * succeeded. If it fails (e.g. network issue), call it again to retry.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitInitialize`() {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_initialize(
        ),
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
 * Start a private-payment encrypted link as the initiator.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitInitiateEncryptedLink`(`secretKeyHex`: kotlin.String, `receiverPublicKey`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_initiate_encrypted_link(
            FfiConverterString.lower(`secretKeyHex`),
            FfiConverterString.lower(`receiverPublicKey`),
        ),
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
 * Returns `true` if an authenticated session is currently active.
 */
public suspend fun `paykitIsAuthenticated`(): kotlin.Boolean {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_is_authenticated(
        ),
        { future, callback, continuation -> UniffiLib.ffi_paykit_rust_future_poll_i8(future, callback, continuation) },
        { future, continuation -> UniffiLib.ffi_paykit_rust_future_complete_i8(future, continuation) },
        { future -> UniffiLib.ffi_paykit_rust_future_free_i8(future) },
        { future -> UniffiLib.ffi_paykit_rust_future_cancel_i8(future) },
        // lift function
        { FfiConverterBoolean.lift(it) },
        // Error FFI converter
        UniffiNullRustCallStatusErrorHandler,
    )
}

/**
 * Remove a payment endpoint for the authenticated user.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitRemovePaymentEndpoint`(`methodId`: kotlin.String) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_remove_payment_endpoint(
            FfiConverterString.lower(`methodId`),
        ),
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
 * Restore an established encrypted link from a serialized snapshot.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitRestoreEncryptedLink`(`secretKeyHex`: kotlin.String, `snapshotHex`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_restore_encrypted_link(
            FfiConverterString.lower(`secretKeyHex`),
            FfiConverterString.lower(`snapshotHex`),
        ),
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
 * Restore an in-progress encrypted-link handshake from a serialized snapshot.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitRestoreEncryptedLinkHandshake`(`secretKeyHex`: kotlin.String, `snapshotHex`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_restore_encrypted_link_handshake(
            FfiConverterString.lower(`secretKeyHex`),
            FfiConverterString.lower(`snapshotHex`),
        ),
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
 * Serialize an established encrypted link snapshot for durable storage.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSerializeEncryptedLink`(`linkId`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_serialize_encrypted_link(
            FfiConverterString.lower(`linkId`),
        ),
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
 * Serialize an in-progress handshake snapshot for durable storage.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSerializeEncryptedLinkHandshake`(`handshakeId`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_serialize_encrypted_link_handshake(
            FfiConverterString.lower(`handshakeId`),
        ),
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
 * Configure automatic recovery attempts for a pending encrypted-link handshake.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSetEncryptedLinkHandshakeMaxRecoveryAttempts`(`handshakeId`: kotlin.String, `max`: kotlin.UInt) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_set_encrypted_link_handshake_max_recovery_attempts(
            FfiConverterString.lower(`handshakeId`),
            FfiConverterUInt.lower(`max`),
        ),
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
 * Configure automatic send retries for an established encrypted link.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSetEncryptedLinkMaxSendRetries`(`linkId`: kotlin.String, `max`: kotlin.UInt) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_set_encrypted_link_max_send_retries(
            FfiConverterString.lower(`linkId`),
            FfiConverterUInt.lower(`max`),
        ),
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
 * Publish or update a payment endpoint for the authenticated user.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSetPaymentEndpoint`(`methodId`: kotlin.String, `endpointData`: kotlin.String) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_set_payment_endpoint(
            FfiConverterString.lower(`methodId`),
            FfiConverterString.lower(`endpointData`),
        ),
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
 * Encrypt and send the complete private payments envelope over an established link.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSetPrivatePayments`(`linkId`: kotlin.String, `payload`: FfiPrivatePaymentsPayload) {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_set_private_payments(
            FfiConverterString.lower(`linkId`),
            FfiConverterTypeFfiPrivatePaymentsPayload.lower(`payload`),
        ),
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
 * Sign in with a raw secret key. Only available with the `dev-auth`
 * feature (enabled by default, disable for production builds).
 *
 * The homeserver is resolved automatically via PKDNS.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSignIn`(`secretKeyHex`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_sign_in(
            FfiConverterString.lower(`secretKeyHex`),
        ),
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
 * End the current session on the homeserver and clear local state.
 *
 * If the server request fails the session is restored so no data is lost.
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSignOut`() {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_sign_out(
        ),
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
 * Sign up for a new account using a raw secret key. Only available with
 * the `dev-auth` feature (enabled by default, disable for production builds).
 */
@Throws(PaykitFfiException::class, kotlin.coroutines.cancellation.CancellationException::class)
public suspend fun `paykitSignUp`(`secretKeyHex`: kotlin.String, `homeserverPublicKey`: kotlin.String): kotlin.String {
    return uniffiRustCallAsync(
        UniffiLib.uniffi_paykit_fn_func_paykit_sign_up(
            FfiConverterString.lower(`secretKeyHex`),
            FfiConverterString.lower(`homeserverPublicKey`),
        ),
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