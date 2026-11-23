package com.mroz59.hestia.rs;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;

/**
 * Keeps the app's network while a PIN pairing waits for the user, who
 * usually leaves the app to type the PIN in the host's web page. Without a
 * foreground service Android cuts a backgrounded app's sockets and the
 * pairing request fails ("Software caused connection abort"). A short
 * service (Android 14+) ends by itself after about three minutes at most;
 * the app stops it as soon as pairing finishes.
 */
public final class PairingService extends Service {
    private static final String CHANNEL = "pairing";
    private static final int NOTIFICATION_ID = 1;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        if (manager != null) {
            manager.createNotificationChannel(
                    new NotificationChannel(CHANNEL, "Pairing", NotificationManager.IMPORTANCE_LOW));
        }
        Notification notification = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle("Pairing with your PC")
                .setContentText("Enter the PIN on the host, then come back")
                .setOngoing(true)
                .build();
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE);
            } else {
                startForeground(NOTIFICATION_ID, notification);
            }
        } catch (RuntimeException e) {
            // Not allowed now (e.g. started from the background): pairing
            // still runs, it just isn't protected.
            stopSelf();
        }
        return START_NOT_STICKY;
    }

    @Override
    public void onTimeout(int startId) {
        stopSelf();
    }
}
