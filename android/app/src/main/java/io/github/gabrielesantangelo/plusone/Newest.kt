package io.github.gabrielesantangelo.plusone

/**
 * What the whole tablet has one of, which each session of a feature makes
 * its own (Wi-Fi Direct's group, the network the process is bound to): the
 * newest session's. One before it, ending, lets it go only if no newer one
 * took it since; else it would undo the newer one's.
 */
class Newest {
    private var holder: Any? = null

    /** session's from now on, if get() (what it does to have it) says it
     * has it: under the lock, after a let-go under way, not during it. */
    @Synchronized
    fun take(session: Any, get: () -> Boolean = { true }): Boolean {
        if (!get()) return false
        holder = session
        return true
    }

    /** let() if session still has it: under the lock, so that no newer
     * session takes it meanwhile. */
    @Synchronized
    fun letGo(session: Any, let: () -> Unit) {
        if (holder !== session) return
        holder = null
        let()
    }
}
