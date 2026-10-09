package com.iameberhard.soundcraft

import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.File
import java.io.FileInputStream

/**
 * The Android shell around the Rust app (`apps/soundcraft-android`, loaded as
 * `libsoundcraft_android.so` by GameActivity's glue). The Rust side calls [pickOpen] and
 * [copyToDownloads] through JNI; a picked file is copied into the app's private `Imports/`
 * folder and its path goes back through [nativeDeliverFile].
 */
class MainActivity : GameActivity() {

    companion object {
        private const val TAG = "soundcraft"
        private const val FOLDER = "SoundCraft"

        init {
            System.loadLibrary("soundcraft_android")
        }
    }

    /** Implemented in Rust: hands a picked file (the command that asked for it, its path) to the app. */
    private external fun nativeDeliverFile(cmd: String, path: String)

    /** The command whose Browse… button opened the picker (`session.open`, `file.import_audio`…). */
    @Volatile
    private var pendingCmd: String = "file.import_audio"

    /** Files this session created in Downloads, so copying the same name again overwrites. */
    private val savedUris = HashMap<String, Uri>()

    private val openDocument = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri: Uri? ->
        if (uri == null) return@registerForActivityResult
        val cmd = pendingCmd
        // Copy off the UI thread; an audio file can be large.
        Thread {
            try {
                val name = displayName(uri) ?: "file"
                val dir = File(filesDir, "Imports").apply { mkdirs() }
                val target = uniqueFile(dir, name)
                val input = contentResolver.openInputStream(uri)
                if (input == null) {
                    toast("Couldn't read $name")
                } else {
                    input.use { src -> target.outputStream().use { dst -> src.copyTo(dst) } }
                    nativeDeliverFile(cmd, target.absolutePath)
                }
            } catch (e: Exception) {
                Log.e(TAG, "open failed", e)
                toast("Couldn't open the file: ${e.message}")
            }
        }.start()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        hideSystemBars()
        deliverIntent(intent)
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        deliverIntent(intent)
    }


    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    /** Full screen for the Edit and Mix windows; a swipe from an edge shows the bars briefly. */
    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    /** A file opened (VIEW) or shared (SEND) from another app: a session opens, audio imports. */
    private fun deliverIntent(intent: Intent?) {
        if (intent == null) return
        @Suppress("DEPRECATION")
        val shared: Uri? = intent.getParcelableExtra(Intent.EXTRA_STREAM)
        val uri: Uri = when (intent.action) {
            Intent.ACTION_VIEW -> intent.data
            Intent.ACTION_SEND -> shared
            else -> null
        } ?: return
        Thread {
            try {
                val name = displayName(uri) ?: "file"
                val cmd = if (name.lowercase().endsWith(".scraft")) "session.open" else "file.import_audio"
                val dir = File(filesDir, "Imports").apply { mkdirs() }
                val target = uniqueFile(dir, name)
                val input = contentResolver.openInputStream(uri)
                if (input == null) {
                    toast("Couldn't read $name")
                } else {
                    input.use { src -> target.outputStream().use { dst -> src.copyTo(dst) } }
                    nativeDeliverFile(cmd, target.absolutePath)
                }
            } catch (e: Exception) {
                Log.e(TAG, "open failed", e)
                toast("Couldn't open the file: ${e.message}")
            }
        }.start()
    }

    // ---- Called from Rust (any thread) --------------------------------------------------------

    /**
     * Browse… in an open/import dialog: the system picker. All files, since sessions (.scraft)
     * and some audio formats have no registered MIME type. [cmd] comes back with the file.
     */
    fun pickOpen(cmd: String) {
        pendingCmd = cmd
        runOnUiThread {
            try {
                openDocument.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /**
     * A saved session, bounce or export: copy the file at [path] to `Downloads/SoundCraft/[name]`
     * through MediaStore. The same name copied again in this session overwrites the file.
     * Returns null on success, else a message.
     */
    fun copyToDownloads(path: String, name: String): String? {
        return try {
            val source = File(path)
            if (!source.isFile) return "$path is not a file"
            val resolver = contentResolver
            val existing = savedUris[name]
            val uri = existing ?: run {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, name)
                    put(MediaStore.Downloads.MIME_TYPE, mimeFor(name))
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/" + FOLDER)
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return "couldn't create $name in Downloads"
            }
            val out = resolver.openOutputStream(uri, "wt") ?: return "couldn't open $name for writing"
            out.use { dst -> FileInputStream(source).use { src -> src.copyTo(dst) } }
            if (existing == null) {
                resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
                savedUris[name] = uri
            }
            toast("Saved to Downloads/$FOLDER/$name")
            null
        } catch (e: Exception) {
            Log.e(TAG, "copy to Downloads failed", e)
            e.message ?: e.toString()
        }
    }

    // ---- Helpers ------------------------------------------------------------------------------

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (i >= 0) return c.getString(i)
            }
        }
        return uri.lastPathSegment
    }

    /** `name`, or `name (2)`, `name (3)`… when a different import already has that name. */
    private fun uniqueFile(dir: File, name: String): File {
        val safe = name.replace('/', '_')
        var f = File(dir, safe)
        if (!f.exists()) return f
        val stem = safe.substringBeforeLast('.')
        val ext = if (safe.contains('.')) "." + safe.substringAfterLast('.') else ""
        var n = 2
        while (f.exists()) {
            f = File(dir, "$stem ($n)$ext")
            n++
        }
        return f
    }

    private fun mimeFor(name: String): String = when (name.substringAfterLast('.', "").lowercase()) {
        "wav", "wave", "bwf" -> "audio/wav"
        "aif", "aiff", "aifc" -> "audio/aiff"
        "flac" -> "audio/flac"
        "mp3" -> "audio/mpeg"
        "ogg", "oga" -> "audio/ogg"
        "mid", "midi", "smf" -> "audio/midi"
        "txt" -> "text/plain"
        "svg" -> "image/svg+xml"
        "musicxml" -> "application/vnd.recordare.musicxml+xml"
        else -> "application/octet-stream"
    }

    private fun toast(text: String) {
        runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
    }
}
