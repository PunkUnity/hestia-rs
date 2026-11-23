package com.mroz59.hestia;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.pm.ActivityInfo;
import android.graphics.PixelFormat;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.os.Build;
import android.os.PersistableBundle;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.text.InputType;
import android.view.Gravity;
import android.view.InputDevice;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;

/**
 * Hestia RS's Android glue that needs the UI thread or a Java class:
 * immersive landscape streaming and a soft keyboard that types straight
 * into the host (the way Artemis does it, no text box).
 *
 * Loaded at runtime from a dex embedded in the native library; the native
 * methods are registered from Rust.
 */
public final class HestiaHelper {
    private HestiaHelper() {}

    /** Text the keyboard committed (accented letters, emoji, swipe words…). */
    static native void nativeCommitText(String text);

    /** The keyboard deleted `count` characters before the cursor. */
    static native void nativeDeleteBefore(int count);

    /** The direct video surface appeared, or went away (null). */
    static native void nativeVideoSurface(Surface surface);

    /** A regular decoder that can accept the requested stream mode. */
    public static String findVideoDecoder(String mime, int width, int height, int fps) {
        for (MediaCodecInfo codec : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
            if (codec.isEncoder()) {
                continue;
            }
            String name = codec.getName();
            if (Build.VERSION.SDK_INT >= 29) {
                if (!codec.isHardwareAccelerated() || codec.isSoftwareOnly()) {
                    continue;
                }
            } else if (name.startsWith("OMX.google.") || name.startsWith("c2.android.")) {
                // Older Android releases do not identify hardware codecs.
                continue;
            }
            for (String type : codec.getSupportedTypes()) {
                if (!mime.equalsIgnoreCase(type)) {
                    continue;
                }
                try {
                    MediaCodecInfo.CodecCapabilities caps = codec.getCapabilitiesForType(type);
                    MediaCodecInfo.VideoCapabilities video = caps.getVideoCapabilities();
                    if (video != null
                            && !caps.isFeatureRequired(MediaCodecInfo.CodecCapabilities.FEATURE_SecurePlayback)
                            && !caps.isFeatureRequired(MediaCodecInfo.CodecCapabilities.FEATURE_TunneledPlayback)
                            && video.areSizeAndRateSupported(width, height, fps)) {
                        return name;
                    }
                } catch (IllegalArgumentException ignored) {
                    // A broken vendor entry should not hide other decoders.
                }
            }
        }
        return null;
    }

    private static KeyboardView sKeyboardView;

    /**
     * Streaming on: the chosen orientation (0 landscape, 1 portrait, either
     * way up; 2 follow the device), system bars hidden until swiped in,
     * screen kept on, drawing under the display cutout.
     */
    public static void setStreamMode(final Activity activity, final boolean on, final int orientation) {
        activity.runOnUiThread(() -> {
            Window window = activity.getWindow();
            int requested = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED;
            if (on && orientation == 0) {
                requested = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE;
            } else if (on && orientation == 1) {
                requested = ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT;
            } else if (on) {
                requested = ActivityInfo.SCREEN_ORIENTATION_FULL_USER;
            }
            activity.setRequestedOrientation(requested);
            if (on) {
                window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
            if (Build.VERSION.SDK_INT >= 28) {
                WindowManager.LayoutParams attrs = window.getAttributes();
                attrs.layoutInDisplayCutoutMode = on
                        ? WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
                        : WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_DEFAULT;
                window.setAttributes(attrs);
            }
            if (Build.VERSION.SDK_INT >= 30) {
                window.setDecorFitsSystemWindows(!on);
                WindowInsetsController controller = window.getInsetsController();
                if (controller != null) {
                    if (on) {
                        controller.setSystemBarsBehavior(
                                WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                        controller.hide(WindowInsets.Type.systemBars());
                    } else {
                        controller.show(WindowInsets.Type.systemBars());
                    }
                }
            } else {
                int flags = View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN;
                window.getDecorView().setSystemUiVisibility(on ? flags : 0);
            }
        });
    }

    private static VideoView sVideoView;

    /**
     * Direct video on: a SurfaceView behind the native window, which the
     * decoder renders to and the system composes itself (often as a
     * hardware overlay), sized to the stream's aspect ratio and centred.
     * The window becomes translucent so the UI drawn over it can leave the
     * stream area clear. Off: the view is removed (its surface is destroyed,
     * which tells the decoder through nativeVideoSurface(null)).
     */
    public static void setDirectVideo(final Activity activity, final boolean on,
            final int width, final int height, final int fps) {
        activity.runOnUiThread(() -> {
            if (sVideoView != null) {
                ViewGroup parent = (ViewGroup) sVideoView.getParent();
                if (parent != null) {
                    parent.removeView(sVideoView);
                }
                sVideoView = null;
            }
            if (!on) {
                return;
            }
            activity.getWindow().setFormat(PixelFormat.TRANSLUCENT);
            VideoView view = new VideoView(activity, width, height, fps);
            sVideoView = view;
            activity.addContentView(view, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT,
                    Gravity.CENTER));
        });
    }

    /** The stream's surface: the largest rectangle of its aspect ratio that fits. */
    static final class VideoView extends SurfaceView implements SurfaceHolder.Callback {
        private final int videoWidth;
        private final int videoHeight;
        private final int fps;

        VideoView(Context context, int width, int height, int fps) {
            super(context);
            this.videoWidth = Math.max(width, 1);
            this.videoHeight = Math.max(height, 1);
            this.fps = fps;
            setFocusable(false);
            setZOrderOnTop(false);
            getHolder().addCallback(this);
        }

        @Override
        protected void onMeasure(int widthSpec, int heightSpec) {
            int w = MeasureSpec.getSize(widthSpec);
            int h = MeasureSpec.getSize(heightSpec);
            if ((long) w * videoHeight > (long) h * videoWidth) {
                w = (int) ((long) h * videoWidth / videoHeight);
            } else {
                h = (int) ((long) w * videoHeight / videoWidth);
            }
            setMeasuredDimension(w, h);
        }

        @Override
        public void surfaceCreated(SurfaceHolder holder) {
            Surface surface = holder.getSurface();
            if (Build.VERSION.SDK_INT >= 30 && fps > 0) {
                try {
                    surface.setFrameRate(fps, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE);
                } catch (IllegalArgumentException | IllegalStateException ignored) {
                    // Refresh matching is a hint; the stream works without it.
                }
            }
            nativeVideoSurface(surface);
        }

        @Override
        public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {}

        @Override
        public void surfaceDestroyed(SurfaceHolder holder) {
            nativeVideoSurface(null);
        }
    }

    /**
     * PIN pairing on/off: a short foreground service keeps the app's network
     * while the user is in the browser typing the PIN (see PairingService,
     * compiled into the APK, hence started by name).
     */
    public static void setPairingService(final Activity activity, final boolean on) {
        android.content.Intent intent = new android.content.Intent();
        intent.setClassName(activity.getPackageName(), "com.mroz59.hestia.rs.PairingService");
        try {
            if (on) {
                activity.startForegroundService(intent);
            } else {
                activity.stopService(intent);
            }
        } catch (RuntimeException ignored) {
            // Pairing still runs without it.
        }
    }

    /**
     * Rumble from the host: on the controller's own motor when it has one,
     * otherwise on the phone. Strengths are 0-65535; 0 and 0 stops it.
     * GameStream rumble is a level that holds until changed, hence the
     * long one-shot that the next call replaces or cancels.
     */
    public static void rumble(final Activity activity, final int deviceId, final int low, final int high) {
        Vibrator vibrator = null;
        if (deviceId >= 0) {
            InputDevice device = InputDevice.getDevice(deviceId);
            if (device != null) {
                Vibrator own = device.getVibrator();
                if (own != null && own.hasVibrator()) {
                    vibrator = own;
                }
            }
        }
        if (vibrator == null) {
            vibrator = (Vibrator) activity.getSystemService(Context.VIBRATOR_SERVICE);
        }
        if (vibrator == null || !vibrator.hasVibrator()) {
            return;
        }
        int strength = Math.max(low, high) >> 8;
        if (strength <= 0) {
            vibrator.cancel();
            return;
        }
        int amplitude = vibrator.hasAmplitudeControl()
                ? Math.max(1, Math.min(255, strength))
                : VibrationEffect.DEFAULT_AMPLITUDE;
        vibrator.vibrate(VibrationEffect.createOneShot(60_000, amplitude));
    }

    /** Shows or hides the soft keyboard that types into the host. */
    public static void setKeyboardVisible(final Activity activity, final boolean visible) {
        activity.runOnUiThread(() -> {
            InputMethodManager imm =
                    (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
            if (imm == null) {
                return;
            }
            KeyboardView view = keyboardView(activity);
            if (visible) {
                view.setFocusable(true);
                view.setFocusableInTouchMode(true);
                view.requestFocus();
                imm.restartInput(view);
                imm.showSoftInput(view, 0);
            } else {
                imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
                view.clearFocus();
                view.setFocusable(false);
            }
        });
    }

    private static KeyboardView keyboardView(Activity activity) {
        if (sKeyboardView == null || sKeyboardView.getContext() != activity) {
            sKeyboardView = new KeyboardView(activity);
            activity.addContentView(sKeyboardView, new ViewGroup.LayoutParams(1, 1));
        }
        return sKeyboardView;
    }

    /**
     * An invisible view the keyboard types into. Its input type is "null",
     * so keyboards send plain key events for ordinary keys (those reach the
     * native side like a hardware keyboard's); what they can only send as
     * text, or deletions, comes through this InputConnection.
     */
    static final class KeyboardView extends View {
        KeyboardView(Context context) {
            super(context);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = InputType.TYPE_NULL;
            // No full-screen editor in landscape; no action button.
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_ACTION_NONE;
            return new BaseInputConnection(this, false) {
                @Override
                public boolean commitText(CharSequence text, int newCursorPosition) {
                    if (text != null && text.length() > 0) {
                        nativeCommitText(text.toString());
                    }
                    return true;
                }

                @Override
                public boolean deleteSurroundingText(int beforeLength, int afterLength) {
                    if (beforeLength > 0) {
                        nativeDeleteBefore(beforeLength);
                    }
                    return true;
                }
            };
        }
    }

    // ---- Clipboard sync -------------------------------------------------

    private static ClipboardManager clipboard(Activity activity) {
        return (ClipboardManager) activity.getSystemService(Context.CLIPBOARD_SERVICE);
    }

    /**
     * When the clipboard last changed (ms), without reading it: reading
     * another app's clip shows a "pasted from your clipboard" notice on
     * Android 12+, so the text is read only after this moves. 0: empty or
     * not readable now (no focus); -1: unknown.
     */
    public static long clipboardStamp(Activity activity) {
        ClipboardManager cm = clipboard(activity);
        if (cm == null) {
            return -1;
        }
        ClipDescription description = cm.getPrimaryClipDescription();
        if (description == null) {
            return 0;
        }
        return description.getTimestamp();
    }

    /** The clipboard as text, or null. */
    public static String clipboardText(Activity activity) {
        ClipboardManager cm = clipboard(activity);
        if (cm == null || !cm.hasPrimaryClip()) {
            return null;
        }
        ClipData clip = cm.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(activity);
        return text == null ? null : text.toString();
    }

    /**
     * Puts the host's text on the clipboard, marked as coming from another
     * device: Android 13+ then skips its "copied" overlay for it.
     */
    public static void setClipboardText(Activity activity, String text) {
        ClipboardManager cm = clipboard(activity);
        if (cm == null) {
            return;
        }
        ClipData clip = ClipData.newPlainText("Hestia RS", text);
        PersistableBundle extras = new PersistableBundle();
        // ClipDescription.EXTRA_IS_REMOTE_DEVICE (API 33), by value.
        extras.putBoolean("android.content.extra.IS_REMOTE_DEVICE", true);
        clip.getDescription().setExtras(extras);
        cm.setPrimaryClip(clip);
    }
}
