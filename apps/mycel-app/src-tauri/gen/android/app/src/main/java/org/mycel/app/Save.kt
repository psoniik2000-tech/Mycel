package org.mycel.app

import android.content.Context
import android.net.Uri
import java.io.File

object Save {
  /** Writes `from` into the document the system "save as" dialog made: a content:// URI Rust can't open. */
  @JvmStatic
  fun copy(context: Context, from: String, to: String) {
    context.contentResolver.openOutputStream(Uri.parse(to), "wt")!!.use { out ->
      File(from).inputStream().use { it.copyTo(out) }
    }
  }
}
