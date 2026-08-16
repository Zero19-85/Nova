package com.nova.echo

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * Keeps the streaming threads alive while Echo is not the foreground app.
 *
 * ## Why this has to exist
 *
 * Android suspends a backgrounded app's threads regardless of what its sockets
 * are doing. Echo's liveness is entirely thread-driven — a 500 ms media
 * keepalive and a 25 s STUN keepalive — so a suspended app stops talking, the
 * host's tunnel sweep reclaims the slot as dead, and the NAT mapping lapses
 * behind it. Observed live on 2026-08-15: switching away to type a message
 * produced 121 seconds of silence and a reaped session, while the host logged a
 * perfectly healthy stream throughout.
 *
 * A foreground service is the sanctioned way to say "these threads are the
 * point of the app". The manifest has declared the permissions for it since the
 * first draft; only the service itself was missing.
 *
 * ## Deliberately does nothing else
 *
 * It owns no sockets, no decoder, and no handle — [EchoController] keeps all of
 * that. This exists purely so the process keeps its scheduling priority, which
 * is why it is safe for the controller to start and stop it freely.
 */
class EchoService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = buildNotification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // API 34+ requires the type at startForeground time, and it must be
            // covered by a matching <service android:foregroundServiceType>.
            startForeground(ID, notification, foregroundTypes())
        } else {
            startForeground(ID, notification)
        }
        // START_NOT_STICKY: if the system kills us, the session is gone with the
        // native handle anyway. Being restarted without one would put up a
        // notification for a stream that does not exist.
        return START_NOT_STICKY
    }

    /**
     * The foreground types this service may legally claim right now.
     *
     * `microphone` is added only when `RECORD_AUDIO` has actually been granted.
     * API 34+ throws a `SecurityException` for a type whose permission the app
     * does not hold, so claiming it unconditionally would crash every session on
     * a device where the user declined the microphone — turning an optional
     * feature into a fatal one.
     *
     * The consequence worth knowing: a user who grants the permission *after*
     * the service is running gets the upgraded type only when the service is
     * started again, which [EchoController] does at that moment.
     */
    private fun foregroundTypes(): Int {
        var types = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        val granted = checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        if (granted) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        }
        return types
    }

    private fun buildNotification(): Notification {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        // IMPORTANCE_LOW: visible and silent. A streaming session should not
        // make a sound to announce itself.
        val channel = NotificationChannel(CHANNEL, "Streaming", NotificationManager.IMPORTANCE_LOW)
        channel.setShowBadge(false)
        manager.createNotificationChannel(channel)

        val tap = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )

        return Notification.Builder(this, CHANNEL)
            .setContentTitle("Echo is streaming")
            .setContentText("Tap to return to the stream")
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setContentIntent(tap)
            .setOngoing(true)
            .build()
    }

    companion object {
        private const val CHANNEL = "echo.stream"
        private const val ID = 1

        /** Start keeping the process alive. Safe to call when already running. */
        fun start(context: Context) {
            val intent = Intent(context, EchoService::class.java)
            context.startForegroundService(intent)
        }

        /** Stop keeping the process alive. Safe to call when not running. */
        fun stop(context: Context) {
            context.stopService(Intent(context, EchoService::class.java))
        }
    }
}
