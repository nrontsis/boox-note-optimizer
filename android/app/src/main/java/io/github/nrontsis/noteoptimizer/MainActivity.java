package io.github.nrontsis.noteoptimizer;

import android.content.ContentValues;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.os.Environment;
import android.provider.MediaStore;
import android.util.Base64;
import android.util.Log;
import android.webkit.*;
import android.widget.Toast;

import androidx.activity.OnBackPressedCallback;
import androidx.appcompat.app.AppCompatActivity;
import androidx.core.content.FileProvider;
import androidx.core.content.IntentCompat;

import java.io.*;

public class MainActivity extends AppCompatActivity {

    private static final String TAG = "NoteOptimizer";
    private static final String APP_URL = "https://nrontsis.github.io/boox-note-optimizer";
    private static final String PROVIDER = "io.github.nrontsis.noteoptimizer.fileprovider";
    private static final String BOOX_NOTES = "com.onyx.android.note";

    private WebView webView;

    private static final String EXPORT_FOLDER = Environment.DIRECTORY_DOWNLOADS + "/Note Optimizer";

    /** Write file to Downloads/Note Optimizer via MediaStore. Cleans folder first. */
    private Uri writeToDownloads(byte[] data, String fileName) throws IOException {
        // Clean all previous exports in our subfolder
        try {
            getContentResolver().delete(
                MediaStore.Downloads.EXTERNAL_CONTENT_URI,
                MediaStore.Downloads.RELATIVE_PATH + "=?",
                new String[]{EXPORT_FOLDER + "/"});
        } catch (Exception ignored) {}

        ContentValues cv = new ContentValues();
        cv.put(MediaStore.Downloads.DISPLAY_NAME, fileName);
        cv.put(MediaStore.Downloads.MIME_TYPE, "application/octet-stream");
        cv.put(MediaStore.Downloads.RELATIVE_PATH, EXPORT_FOLDER);
        Uri uri = getContentResolver().insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, cv);
        if (uri == null) throw new IOException("Failed to create Downloads entry");
        try (OutputStream os = getContentResolver().openOutputStream(uri)) {
            if (os == null) throw new IOException("Failed to open output stream");
            os.write(data);
        }
        return uri;
    }

    /** Try to open a file in Boox Notes via MediaStore URI, then chooser. */
    private void openInBooxNotes(Uri uri) {
        String[] actions = {Intent.ACTION_VIEW, Intent.ACTION_SEND};
        String[] mimes = {"application/zip", "application/x-zip-compressed",
            "application/octet-stream", "*/*"};

        for (String action : actions) {
            for (String mime : mimes) {
                Intent intent = new Intent(action);
                if (Intent.ACTION_VIEW.equals(action)) {
                    intent.setDataAndType(uri, mime);
                } else {
                    intent.setType(mime);
                    intent.putExtra(Intent.EXTRA_STREAM, uri);
                }
                intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                intent.setPackage(BOOX_NOTES);
                try {
                    startActivity(intent);
                    return;
                } catch (Exception ignored) {}
            }
        }

        // Fallback: chooser
        Intent fallback = new Intent(Intent.ACTION_VIEW);
        fallback.setDataAndType(uri, "application/octet-stream");
        fallback.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        startActivity(Intent.createChooser(fallback, "Open with"));
    }

    /* ── JS bridge: receives base64 file data from the web page ── */
    private class AndroidBridge {
        @JavascriptInterface
        public void receiveFile(String base64, String fileName) {
            try {
                byte[] data = Base64.decode(base64, Base64.DEFAULT);
                Uri uri = writeToDownloads(data, fileName);
                openInBooxNotes(uri);
            } catch (Exception e) {
                Log.e(TAG, "receiveFile failed", e);
                runOnUiThread(() ->
                    Toast.makeText(MainActivity.this, "Open failed: " + e.getMessage(), Toast.LENGTH_LONG).show());
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        webView = new WebView(this);
        setContentView(webView);

        WebSettings s = webView.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setAllowFileAccess(true);
        s.setCacheMode(WebSettings.LOAD_DEFAULT);
        webView.addJavascriptInterface(new AndroidBridge(), "Android");

        webView.setWebViewClient(new WebViewClient() {
            @Override
            public void onPageFinished(WebView view, String url) {
                handleInboundShare(getIntent());
            }
        });

        /* ── Intercept blob: downloads → fetch via JS → pass to native ── */
        webView.setDownloadListener((url, userAgent, contentDisposition, mimetype, contentLength) -> {
            if (url.startsWith("blob:")) {
                String fetchJs =
                    "(async function() {" +
                    "  try {" +
                    "    const r = await fetch('" + url.replace("'", "\\'") + "');" +
                    "    const b = await r.blob();" +
                    "    const reader = new FileReader();" +
                    "    reader.onloadend = function() {" +
                    "      const base64 = reader.result.split(',')[1];" +
                    "      Android.receiveFile(base64, " + jsString(guessFileName(contentDisposition)) + ");" +
                    "    };" +
                    "    reader.readAsDataURL(b);" +
                    "  } catch(e) { console.error('blob fetch failed', e); }" +
                    "})();";
                webView.evaluateJavascript(fetchJs, null);
            }
        });

        registerBackHandler();
        webView.loadUrl(APP_URL);
    }

    /* ── Inbound share: .note file → inject into web app cache ── */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleInboundShare(intent);
    }

    private void handleInboundShare(Intent intent) {
        if (intent == null || !Intent.ACTION_SEND.equals(intent.getAction())) return;
        Uri uri = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri.class);
        if (uri == null) return;

        // Clear the intent so we don't re-process it on page reload
        setIntent(new Intent(Intent.ACTION_MAIN));

        try {
            InputStream is = getContentResolver().openInputStream(uri);
            if (is == null) return;
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            int n;
            while ((n = is.read(buf)) != -1) baos.write(buf, 0, n);
            is.close();

            String base64 = Base64.encodeToString(baos.toByteArray(), Base64.NO_WRAP);
            String name = getFileName(uri);

            String js =
                "(async function() {" +
                "  try {" +
                "    const b64 = '" + base64 + "';" +
                "    const binary = atob(b64);" +
                "    const bytes = new Uint8Array(binary.length);" +
                "    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);" +
                "    const blob = new Blob([bytes], {type:'application/octet-stream'});" +
                "    const cache = await caches.open('share-target');" +
                "    await cache.put('/shared-file', new Response(blob, {headers:{'X-Filename':'" + name.replace("'", "\\'") + "'}}));" +
                "    location.reload();" +
                "  } catch(e) { console.error('inbound share failed', e); }" +
                "})();";
            webView.evaluateJavascript(js, null);
        } catch (Exception e) {
            Toast.makeText(this, "Could not read shared file", Toast.LENGTH_LONG).show();
        }
    }

    private String getFileName(Uri uri) {
        String path = uri.getLastPathSegment();
        if (path != null && path.contains("/")) path = path.substring(path.lastIndexOf('/') + 1);
        return (path != null && path.endsWith(".note")) ? path : "shared.note";
    }

    private static String guessFileName(String contentDisposition) {
        if (contentDisposition != null) {
            String[] parts = contentDisposition.split("filename=");
            if (parts.length > 1) {
                String name = parts[1].trim().replaceAll("[\"';]", "");
                if (!name.isEmpty()) return name;
            }
        }
        return "optimized.note";
    }

    private static String jsString(String s) {
        return "'" + s.replace("\\", "\\\\").replace("'", "\\'") + "'";
    }

    private void registerBackHandler() {
        getOnBackPressedDispatcher().addCallback(this, new OnBackPressedCallback(true) {
            @Override
            public void handleOnBackPressed() {
                if (webView.canGoBack()) webView.goBack();
                else {
                    setEnabled(false);
                    getOnBackPressedDispatcher().onBackPressed();
                }
            }
        });
    }
}
