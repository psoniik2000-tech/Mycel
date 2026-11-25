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
 * the phone shows "new message" and the app pulls and decrypts it when opened.
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

  fun clear(c: Context) = c.getSystemService(NotificationManager::class.java).cancel(NOTE_ID)

  private const val NOTE_ID = 2

  fun notifyNew(c: Context) {
    val nm = c.getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(NotificationChannel("msg", "Сообщения", NotificationManager.IMPORTANCE_HIGH))
    }
    val open = PendingIntent.getActivity(
      c, 0, Intent(c, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
    nm.notify(NOTE_ID, NotificationCompat.Builder(c, "msg")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Mycel")
      .setContentText("Новое сообщение")
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
    Push.notifyNew(this)
  }
}
