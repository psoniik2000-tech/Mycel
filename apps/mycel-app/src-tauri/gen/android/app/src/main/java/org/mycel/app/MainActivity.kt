package org.mycel.app

import android.content.Intent
import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Edge-to-edge: keep the WebView clear of the bars, the notch and the keyboard (no adjustResize here).
    ViewCompat.setOnApplyWindowInsetsListener(findViewById<View>(android.R.id.content)) { v, insets ->
      val i = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime())
      v.setPadding(i.left, i.top, i.right, i.bottom)
      WindowInsetsCompat.CONSUMED
    }
    ContextCompat.startForegroundService(this, Intent(this, NodeService::class.java))
    Push.fetchToken(this)
  }

  override fun onResume() {
    super.onResume()
    Push.clear(this)
  }
}
