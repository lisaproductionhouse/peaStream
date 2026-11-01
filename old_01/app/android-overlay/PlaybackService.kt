package com.fbaudio.live

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import androidx.core.app.NotificationCompat

/**
 * Dịch vụ foreground (loại mediaPlayback): giữ tiến trình sống, CPU và Wi-Fi thức khi tắt màn hình.
 *
 * Âm thanh vẫn do lõi Rust giải mã và phát qua Oboe/AAudio; dịch vụ này KHÔNG nằm trên đường dữ
 * liệu âm thanh nên không thêm bất kỳ độ trễ nào. Cái giá phải trả duy nhất là pin (CPU + Wi-Fi
 * không được ngủ).
 */
class PlaybackService : Service() {
  private var cpuLock: PowerManager.WakeLock? = null
  private var wifiLock: WifiManager.WifiLock? = null
  private val handler = Handler(Looper.getMainLooper())
  private var idleTicks = 0

  // Hàm JNI do lõi Rust cung cấp (app/src-tauri/src/lib.rs).
  private external fun nativeStop()
  private external fun nativeIsActive(): Boolean

  /** Nếu lõi không còn phát (hết phiên/lỗi) 2 lần liên tiếp → tự tắt để khỏi giữ wake lock vô ích. */
  private val watchdog = object : Runnable {
    override fun run() {
      val active = try { nativeIsActive() } catch (_: UnsatisfiedLinkError) { true }
      idleTicks = if (active) 0 else idleTicks + 1
      if (idleTicks >= 2) stopSelf() else handler.postDelayed(this, 4000)
    }
  }

  override fun onCreate() {
    super.onCreate()
    if (Build.VERSION.SDK_INT >= 26) {
      val ch = NotificationChannel(CHANNEL, "Đang phát Facebook Live", NotificationManager.IMPORTANCE_LOW)
      ch.setShowBadge(false)
      getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
    }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action == ACTION_STOP) {
      try { nativeStop() } catch (_: UnsatisfiedLinkError) { }
      stopSelf()
      return START_NOT_STICKY
    }

    val title = intent?.getStringExtra("title") ?: "Facebook Live"
    val launch = packageManager.getLaunchIntentForPackage(packageName)
      ?: Intent(this, MainActivity::class.java)
    val open = PendingIntent.getActivity(this, 0, launch, PendingIntent.FLAG_IMMUTABLE)
    val stop = PendingIntent.getService(
      this, 1, Intent(this, PlaybackService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE
    )
    val notification: Notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(android.R.drawable.ic_media_play)
      .setContentTitle(title)
      .setContentText("Đang phát âm thanh")
      .setContentIntent(open)
      .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Dừng", stop)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setCategory(NotificationCompat.CATEGORY_TRANSPORT)
      .build()

    if (Build.VERSION.SDK_INT >= 29) {
      startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
    } else {
      startForeground(NOTIF_ID, notification)
    }
    acquireLocks()

    idleTicks = 0
    handler.removeCallbacks(watchdog)
    handler.postDelayed(watchdog, 6000) // chừa thời gian để giao diện kịp gọi `start` của Rust
    return START_NOT_STICKY
  }

  private fun acquireLocks() {
    if (cpuLock == null) {
      val pm = getSystemService(POWER_SERVICE) as PowerManager
      cpuLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "fbaudio:cpu").apply {
        setReferenceCounted(false)
        acquire()
      }
    }
    if (wifiLock == null) {
      val wm = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
      @Suppress("DEPRECATION")
      val mode = if (Build.VERSION.SDK_INT >= 29) WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                 else WifiManager.WIFI_MODE_FULL_HIGH_PERF
      wifiLock = wm.createWifiLock(mode, "fbaudio:wifi").apply {
        setReferenceCounted(false)
        acquire()
      }
    }
  }

  override fun onDestroy() {
    handler.removeCallbacks(watchdog)
    cpuLock?.takeIf { it.isHeld }?.release()
    wifiLock?.takeIf { it.isHeld }?.release()
    cpuLock = null
    wifiLock = null
    super.onDestroy()
  }

  override fun onBind(intent: Intent?): IBinder? = null

  companion object {
    private const val CHANNEL = "playback"
    private const val NOTIF_ID = 1
    const val ACTION_STOP = "com.fbaudio.live.STOP"
  }
}
