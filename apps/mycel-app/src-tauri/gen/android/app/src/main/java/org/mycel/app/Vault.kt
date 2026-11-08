package org.mycel.app

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Seals space passwords with an AES key kept in the Android Keystore; called from Rust (vault.rs). */
object Vault {
  private fun key(): SecretKey {
    val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (ks.getKey("mycel", null) as? SecretKey)?.let { return it }
    val g = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    g.init(KeyGenParameterSpec.Builder("mycel", KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
      .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
      .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
      .build())
    return g.generateKey()
  }

  /** iv (12 bytes) + ciphertext. */
  @JvmStatic fun seal(data: ByteArray): ByteArray {
    val c = Cipher.getInstance("AES/GCM/NoPadding")
    c.init(Cipher.ENCRYPT_MODE, key())
    return c.iv + c.doFinal(data)
  }

  @JvmStatic fun open(data: ByteArray): ByteArray {
    val c = Cipher.getInstance("AES/GCM/NoPadding")
    c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, data, 0, 12))
    return c.doFinal(data, 12, data.size - 12)
  }
}
