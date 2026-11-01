package com.fbaudio.live

import android.Manifest
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat

/**
 * Thay cho MainActivity.kt do `cargo tauri android init` sinh ra. Thêm 3 việc:
 *  1. Cầu nối `window.AndroidBridge` cho giao diện: bật/tắt dịch vụ nền, dán từ clipboard,
 *     nhận link chia sẻ từ app Facebook.
 *  2. Xin quyền hiện thông báo (Android 13+) cho thông báo "Đang phát".
 *  3. Nhận intent ACTION_SEND (Chia sẻ → FB Live Audio).
 */
class MainActivity : TauriActivity() {
  @Volatile private var sharedText: String? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    if (Build.VERSION.SDK_INT >= 33 &&
      checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
    ) {
      requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
    }
    captureShared(intent)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    captureShared(intent)
  }

  private fun captureShared(i: Intent?) {
    if (i?.action == Intent.ACTION_SEND && i.type?.startsWith("text/") == true) {
      sharedText = i.getStringExtra(Intent.EXTRA_TEXT)
    }
  }

  // Tauri gọi khi WebView vừa được tạo.
  override fun onWebViewCreate(webView: WebView) {
    webView.addJavascriptInterface(Bridge(), "AndroidBridge")
  }

  private inner class Bridge {
    @JavascriptInterface
    fun startService(title: String) {
      val i = Intent(this@MainActivity, PlaybackService::class.java).putExtra("title", title)
      ContextCompat.startForegroundService(this@MainActivity, i)
    }

    @JavascriptInterface
    fun stopService() {
      this@MainActivity.stopService(Intent(this@MainActivity, PlaybackService::class.java))
    }

    @JavascriptInterface
    fun paste(): String {
      val cm = this@MainActivity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
      val clip = cm.primaryClip ?: return ""
      if (clip.itemCount == 0) return ""
      return clip.getItemAt(0).coerceToText(this@MainActivity)?.toString() ?: ""
    }

    @JavascriptInterface
    fun takeSharedText(): String {
      val t = sharedText ?: ""
      sharedText = null
      return t
    }
  }
}
