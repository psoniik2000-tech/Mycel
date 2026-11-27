package org.mycel.app

import android.content.Context
import android.content.Intent
import androidx.core.content.FileProvider
import java.io.File

/** In-app update (update.rs): hands the downloaded APK to the system installer. Called from Rust over JNI by name. */
object Update {
  @JvmStatic
  fun install(context: Context, path: String) {
    // cache-path in res/xml/file_paths.xml covers the app's cache dir, where the APK is downloaded.
    val uri = FileProvider.getUriForFile(context, context.packageName + ".fileprovider", File(path))
    val intent = Intent(Intent.ACTION_VIEW)
      .setDataAndType(uri, "application/vnd.android.package-archive")
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
    context.startActivity(intent)
  }
}
