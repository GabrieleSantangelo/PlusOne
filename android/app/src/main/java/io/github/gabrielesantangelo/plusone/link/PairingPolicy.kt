package io.github.gabrielesantangelo.plusone.link

/**
 * Who may go on past the hello on the listener: a PC this tablet knows,
 * always; another only to pair, and only while the user holds the pairing
 * window open here ("Pair a new PC", Pairing.windowUntil). Anyone else on
 * the network could otherwise put a pairing question on the screen at any
 * time, and learn the tablet's name and app from its hello (CC-03, CC-07).
 */
object PairingPolicy {
    fun allowed(now: Long, windowUntil: Long, known: Boolean, wantsPair: Boolean): Boolean =
        known || (wantsPair && now < windowUntil)
}
