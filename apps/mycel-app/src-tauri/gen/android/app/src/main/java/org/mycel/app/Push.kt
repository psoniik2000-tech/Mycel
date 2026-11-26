package org.mycel.app

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ProcessLifecycleOwner
import com.google.firebase.FirebaseApp
import com.google.firebase.FirebaseOptions
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import java.io.File

/** Initializes Firebase in every process start (also when a push starts the app). */
class MycelApp : Application() {
  override fun onCreate() {
    super.onCreate()
    Push.init(this)
  }
}

/**
 * FCM wake-ups (DESIGN §7.5): the mailbox sends a content-free push when something new is posted;
 * a node alive in this process pulls at once and notifies with the text; otherwise a remembered space
 * is opened without UI for a few seconds to do the same; without one the phone shows "new message".
 * The token goes to a file the Rust side reads at unlock and registers with the mailbox.
 */
object Push {
  // Firebase project "mycel" (client config, not a secret); set here instead of the google-services plugin.
  fun init(c: Context) {
    if (FirebaseApp.getApps(c).isNotEmpty()) return
    FirebaseApp.initializeApp(c, FirebaseOptions.Builder()
      .setProjectId("mycel-30a91")
      .setApplicationId("1:483862917880:android:81cc347ef21fd3b7e6810d")
      .setApiKey("AIzaSyCR3DhlE8EINMQigek2COERljhqt_44dI4")
      .setGcmSenderId("483862917880")
      .build())
  }

  fun fetchToken(c: Context) {
    // No Google Play services: no pushes, the app still pulls when open.
    runCatching { FirebaseMessaging.getInstance().token.addOnSuccessListener { save(c, it) } }
  }

  fun save(c: Context, token: String) = File(c.dataDir, "push_token").writeText(token)

  // Also the per-message ones from `pullNode`; the foreground service's notice stays.
  fun clear(c: Context) = c.getSystemService(NotificationManager::class.java).cancelAll()

  private const val NOTE_ID = 2

  /** lib.rs: an open space pulls now and notifies with the text; false = no node in this process. */
  @JvmStatic external fun wake(): Boolean

  // A process the push started has no Rust library loaded (and no node): UnsatisfiedLinkError → false.
  fun wakeNode() = runCatching { wake() }.getOrDefault(false)

  /** lib.rs: opens the last space with its remembered password, pulls, confirms delivery, closes.
   *  Title, body pairs of the new messages; null = no remembered password (or it failed). */
  @JvmStatic external fun pull(root: String, cache: String): Array<String>?

  // false → show the content-free notice.
  fun pullNode(c: Context): Boolean = runCatching {
    System.loadLibrary("mycel_app_lib")
    val m = pull(c.dataDir.path, c.cacheDir.path) ?: return false
    val id = (System.currentTimeMillis() % 100_000).toInt() * 100 + 1000  // clear of NOTE_ID and the service's 1
    for (i in m.indices step 2) notifyNew(c, m[i], m[i + 1], id + i / 2)
    true
  }.getOrDefault(false)

  fun notifyNew(c: Context, title: String = "Mycel", text: String = "Новое сообщение", id: Int = NOTE_ID) {
    val nm = c.getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(NotificationChannel("msg", "Сообщения", NotificationManager.IMPORTANCE_HIGH))
    }
    val open = PendingIntent.getActivity(
      c, 0, Intent(c, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
    nm.notify(id, NotificationCompat.Builder(c, "msg")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(title)
      .setContentText(text)
      .setAutoCancel(true)
      .setContentIntent(open)
      .build())
  }
}

class PushService : FirebaseMessagingService() {
  override fun onNewToken(token: String) = Push.save(this, token)

  override fun onMessageReceived(m: RemoteMessage) {
    // App on screen: it pulls by itself and shows the message.
    if (ProcessLifecycleOwner.get().lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) return
    if (!Push.wakeNode() && !Push.pullNode(this)) Push.notifyNew(this)
  }
}
