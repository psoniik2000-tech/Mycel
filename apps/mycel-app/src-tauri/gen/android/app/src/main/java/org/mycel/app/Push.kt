package org.mycel.app

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.Person
import androidx.core.app.RemoteInput
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ProcessLifecycleOwner
import com.google.firebase.FirebaseApp
import com.google.firebase.FirebaseOptions
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import java.io.File
import kotlin.concurrent.thread

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
   *  Space, chat, title, body of each new message; null = no remembered password (or it failed). */
  @JvmStatic external fun pull(root: String, cache: String): Array<String>?

  // false → show the content-free notice.
  fun pullNode(c: Context): Boolean = runCatching {
    System.loadLibrary("mycel_app_lib")
    val m = pull(c.dataDir.path, c.cacheDir.path) ?: return false
    for (i in m.indices step 4) show(c, m[i], m[i + 1], m[i + 2], m[i + 3])
    true
  }.getOrDefault(false)

  /** lib.rs: Reply (`text`) or Read (null) in a chat, through the node in this process or a space opened
   *  without UI; false = failed. */
  @JvmStatic external fun act(root: String, cache: String, space: String, chat: String, text: String?): Boolean

  private fun channel(c: Context): NotificationManager {
    val nm = c.getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(NotificationChannel("msg", "Сообщения", NotificationManager.IMPORTANCE_HIGH))
    }
    return nm
  }

  private fun open(c: Context) = PendingIntent.getActivity(
    c, 0, Intent(c, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)

  fun notifyNew(c: Context) {
    channel(c).notify(NOTE_ID, NotificationCompat.Builder(c, "msg")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Mycel")
      .setContentText("Новое сообщение")
      .setAutoCancel(true)
      .setContentIntent(open(c))
      .build())
  }

  /** One notification per chat (its messages pile up) with Reply and Read; also from lib.rs `notify`. */
  @JvmStatic fun show(c: Context, space: String, chat: String, title: String, text: String) {
    val nm = channel(c)
    val id = ("$space/$chat".hashCode() and 0xFFFFFFF) + 1000  // clear of NOTE_ID and the service's 1
    val style = nm.activeNotifications.find { it.id == id }
      ?.let { NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(it.notification) }
      ?: NotificationCompat.MessagingStyle(Person.Builder().setName("Вы").build())
    style.addMessage(text, System.currentTimeMillis(), Person.Builder().setName(title).build())
    // The action makes the two intents distinct; the reply one must be mutable to carry the typed text.
    fun intent(action: String, flag: Int) = PendingIntent.getBroadcast(c, id,
      Intent(c, ActionReceiver::class.java).setAction(action)
        .putExtra("space", space).putExtra("chat", chat).putExtra("id", id),
      PendingIntent.FLAG_UPDATE_CURRENT or flag)
    val reply = NotificationCompat.Action.Builder(0, "Ответить", intent("reply", PendingIntent.FLAG_MUTABLE))
      .addRemoteInput(RemoteInput.Builder("text").setLabel("Сообщение").build())
      .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_REPLY)
      .setShowsUserInterface(false)
      .build()
    val read = NotificationCompat.Action.Builder(0, "Прочитано", intent("read", PendingIntent.FLAG_IMMUTABLE))
      .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ)
      .setShowsUserInterface(false)
      .build()
    nm.notify(id, NotificationCompat.Builder(c, "msg")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setStyle(style)
      .setAutoCancel(true)
      .setContentIntent(open(c))
      .addAction(reply)
      .addAction(read)
      .build())
  }
}

/** Reply / Read on a message notification (`Push.show`): done without opening the app. */
class ActionReceiver : BroadcastReceiver() {
  override fun onReceive(c: Context, i: Intent) {
    val space = i.getStringExtra("space") ?: return
    val chat = i.getStringExtra("chat") ?: return
    val text = if (i.action == "reply") {
      RemoteInput.getResultsFromIntent(i)?.getCharSequence("text")?.toString()?.takeIf { it.isNotBlank() } ?: return
    } else null
    val done = goAsync()
    thread {
      val ok = runCatching {
        System.loadLibrary("mycel_app_lib")
        Push.act(c.dataDir.path, c.cacheDir.path, space, chat, text)
      }.getOrDefault(false)
      c.getSystemService(NotificationManager::class.java).cancel(i.getIntExtra("id", 0))
      if (!ok && text != null) Push.show(c, space, chat, "Mycel", "Не отправлено: $text")
      done.finish()
    }
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
