package io.qpwgraph.relay

import android.content.SharedPreferences
import java.lang.reflect.Proxy

/**
 * A `SharedPreferences` backed by a map, built with a dynamic proxy so these
 * tests need no Android runtime.
 *
 * Only the accessors the relay uses are implemented. [commitSucceeds] models
 * a device that refuses the write, which the installation identity depends on
 * detecting.
 */
class FakePreferences(
    private val values: MutableMap<String, Any?> = mutableMapOf(),
    var commitSucceeds: Boolean = true,
) {
    operator fun get(key: String): Any? = values[key]

    operator fun set(key: String, value: Any?) {
        values[key] = value
    }

    fun proxy(): SharedPreferences {
        val pending = mutableMapOf<String, Any?>()
        val removed = mutableSetOf<String>()
        lateinit var editor: SharedPreferences.Editor
        editor = Proxy.newProxyInstance(
            SharedPreferences.Editor::class.java.classLoader,
            arrayOf(SharedPreferences.Editor::class.java),
        ) { _, method, args ->
            when (method.name) {
                "putString", "putInt", "putLong", "putBoolean" -> {
                    pending[args!![0] as String] = args[1]
                    editor
                }
                "remove" -> {
                    removed += args!![0] as String
                    editor
                }
                "commit", "apply" -> {
                    if (method.name == "apply" || commitSucceeds) {
                        values.putAll(pending)
                        removed.forEach { values.remove(it) }
                        pending.clear()
                        removed.clear()
                    }
                    if (method.name == "commit") commitSucceeds else Unit
                }
                else -> editor
            }
        } as SharedPreferences.Editor

        return Proxy.newProxyInstance(
            SharedPreferences::class.java.classLoader,
            arrayOf(SharedPreferences::class.java),
        ) { _, method, args ->
            when (method.name) {
                "edit" -> editor
                "getString" -> values[args!![0] as String] as String? ?: args[1]
                "getInt" -> values[args!![0] as String] as Int? ?: args[1]
                "getBoolean" -> values[args!![0] as String] as Boolean? ?: args[1]
                "getLong" -> values[args!![0] as String] as Long? ?: args[1]
                "contains" -> values.containsKey(args!![0] as String)
                else -> null
            }
        } as SharedPreferences
    }
}
