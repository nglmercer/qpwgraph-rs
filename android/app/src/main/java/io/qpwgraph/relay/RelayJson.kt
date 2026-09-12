package io.qpwgraph.relay

import org.json.JSONArray
import org.json.JSONObject

/**
 * The JSON shapes the native bridge speaks, parsed in one place.
 *
 * These are pure functions over strings, so they are unit-testable without an
 * Android runtime — which is the point of having them here rather than inline
 * in the ViewModel, where every parse was also entangled with state updates.
 */
object RelayJson {

    /**
     * Read a handle out of a `create` response.
     *
     * @throws IllegalStateException when native answered with an error
     * @throws IllegalArgumentException when the response is not a creation
     *   or carries the null handle
     */
    fun createdHandle(raw: String, nullHandleMessage: () -> String): Long {
        val response = JSONObject(raw)
        if (response.optString("type") == "error") {
            throw IllegalStateException(response.optString("message"))
        }
        require(response.optString("type") == "created") {
            "native creation returned an unexpected response"
        }
        return response.optLong("handle").also {
            require(it != 0L) { nullHandleMessage() }
        }
    }

    /** Credentials the native engines are seeded with on creation. */
    fun trustedPeersJson(peers: List<TrustedRelayPeer>): String {
        val array = JSONArray()
        peers.forEach { peer ->
            array.put(
                JSONObject()
                    .put("peer_id", peer.peerId)
                    .put("secret", peer.secret),
            )
        }
        return array.toString()
    }

    /** The discovery snapshot handed to a connected client's resume worker. */
    fun discoveredPeersJson(peers: List<DiscoveredPeer>): String {
        val array = JSONArray()
        peers.forEach { peer ->
            array.put(
                JSONObject()
                    .put("id", peer.id)
                    .put("name", peer.name)
                    .put("address", peer.address)
                    .put("link", peer.link),
            )
        }
        return array.toString()
    }

    fun discoveredPeers(raw: String): List<DiscoveredPeer> {
        val peers = JSONArray(raw)
        return (0 until peers.length()).map { index ->
            val peer = peers.getJSONObject(index)
            DiscoveredPeer(
                id = peer.optString("id"),
                name = peer.optString("name"),
                address = peer.optString("address"),
                link = peer.optString("link"),
            )
        }
    }

    fun sessions(status: JSONObject): List<RelaySessionInfo> {
        val sessions = status.optJSONArray("sessions") ?: JSONArray()
        return (0 until sessions.length()).map { index ->
            val session = sessions.getJSONObject(index)
            RelaySessionInfo(
                id = session.optLong("id"),
                name = session.optString("name"),
                address = session.optString("address"),
                sending = session.optBoolean("sending"),
                receiving = session.optBoolean("receiving"),
                transport = session.optString("transport"),
                link = session.optString("link"),
                controlState = session.optString("control_state"),
                audioChannelState = session.optString("audio_channel_state"),
                trusted = session.optBoolean("trusted"),
            )
        }
    }

    /** `null` when the response is not a link snapshot. */
    fun localLinks(raw: String): List<LocalLinkInfo>? {
        val response = try {
            JSONObject(raw)
        } catch (_: Exception) {
            return null
        }
        if (response.optString("type") != "links") return null
        val links = response.optJSONArray("links") ?: return null
        return (0 until links.length()).map { index ->
            val link = links.getJSONObject(index)
            LocalLinkInfo(
                name = link.optString("name"),
                addr = link.optString("addr"),
                kind = link.optString("kind"),
            )
        }
    }

    /**
     * A poll that returns an object rather than an array is native reporting
     * that the handle is gone. Callers have to tell that apart from a healthy
     * empty poll, so it is recognised here rather than at each call site.
     */
    fun pollError(raw: String): PollError? {
        if (!raw.trimStart().startsWith("{")) return null
        val response = JSONObject(raw)
        if (response.optString("type") != "error") return null
        return PollError(
            message = response.optString("message"),
            code = response.optString("code"),
        )
    }

    data class PollError(val message: String, val code: String) {
        val unknownHandle: Boolean
            get() = code == "unknown_client_handle" || code == "unknown_host_handle"
    }
}
