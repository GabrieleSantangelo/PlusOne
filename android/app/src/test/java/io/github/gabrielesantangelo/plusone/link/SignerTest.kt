package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.KeyPairGenerator
import java.security.Signature
import java.security.spec.ECGenParameterSpec

/** The signer the link's core calls through JNI, for its TLS handshakes. */
class SignerTest {
    @Test
    fun itSignsAsTls13sEcdsaWithP256AndSha256() {
        val keys = KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp256r1")) }.generateKeyPair()
        val signed = LinkCore.Signer(keys.private).sign("hello".toByteArray())!!
        assertTrue(Signature.getInstance("SHA256withECDSA").run {
            initVerify(keys.public); update("hello".toByteArray()); verify(signed)
        })
    }

    @Test
    fun aKeyThatCannotSignGivesNullNotAnExceptionIntoRust() {
        val rsa = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
        assertNull(LinkCore.Signer(rsa.private).sign("hello".toByteArray()))
    }
}
