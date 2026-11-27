package org.mycel.app

import android.content.ActivityNotFoundException
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import java.io.File

object Save {
  /** Writes `from` into the document the system "save as" dialog made: a content:// URI Rust can't open. */
  @JvmStatic
  fun copy(context: Context, from: String, to: String) {
    context.contentResolver.openOutputStream(Uri.parse(to), "wt")!!.use { out ->
      File(from).inputStream().use { it.copyTo(out) }
    }
  }

  /**
   * "Сохранить": copies a received file into the public folder of its kind (Pictures, Movies or
   * Downloads, in a Mycel subfolder) through MediaStore, so no permission is needed on Android 10+.
   * Returns the folder it went to. Below Android 10 the file is copied there directly.
   */
  @JvmStatic
  fun download(context: Context, from: String, name: String, mime: String): String {
    val (collection, dir) = when {
      mime.startsWith("image/") -> MediaStore.Images.Media.EXTERNAL_CONTENT_URI to Environment.DIRECTORY_PICTURES
      mime.startsWith("video/") -> MediaStore.Video.Media.EXTERNAL_CONTENT_URI to Environment.DIRECTORY_MOVIES
      else -> MediaStore.Downloads.EXTERNAL_CONTENT_URI to Environment.DIRECTORY_DOWNLOADS
    }
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
      val folder = File(Environment.getExternalStoragePublicDirectory(dir), "Mycel").apply { mkdirs() }
      File(from).copyTo(File(folder, name), overwrite = true)
      return "$dir/Mycel"
    }
    val values = ContentValues().apply {
      put(MediaStore.MediaColumns.DISPLAY_NAME, name)
      put(MediaStore.MediaColumns.MIME_TYPE, mime)
      put(MediaStore.MediaColumns.RELATIVE_PATH, "$dir/Mycel")
      put(MediaStore.MediaColumns.IS_PENDING, 1)
    }
    val resolver = context.contentResolver
    val uri = resolver.insert(collection, values) ?: throw IllegalStateException("Нет доступа к папке $dir")
    try {
      resolver.openOutputStream(uri)!!.use { out -> File(from).inputStream().use { it.copyTo(out) } }
    } catch (e: Exception) {
      resolver.delete(uri, null, null)
      throw e
    }
    resolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
    return "$dir/Mycel"
  }

  /** "Открыть": shows a received file in an app for its type, through the FileProvider (res/xml/file_paths.xml). */
  @JvmStatic
  fun open(context: Context, path: String, name: String) {
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", File(path))
    val ext = name.substringAfterLast('.', "").lowercase()
    val mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "*/*"
    val intent = Intent(Intent.ACTION_VIEW)
      .setDataAndType(uri, mime)
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
    try {
      context.startActivity(intent)
    } catch (e: ActivityNotFoundException) {
      throw IllegalStateException("Нет приложения для этого типа файла")
    }
  }
}
