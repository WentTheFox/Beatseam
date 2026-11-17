package tf.went.beatseam.spike

import android.os.Bundle
import android.os.Looper
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import uniffi.beatseam_vm.NowPlayingState
import uniffi.beatseam_vm.NowPlayingViewModel
import uniffi.beatseam_vm.StateListener
import uniffi.beatseam_vm.VmException

private const val TAG = "uniffi-spike"

class MainActivity : ComponentActivity() {
    // The view model lives in Rust. Kotlin holds an immutable snapshot and replaces it on every push.
    private lateinit var vm: NowPlayingViewModel
    private var state by mutableStateOf<NowPlayingState?>(null)
    private var pushes = 0
    private var pushesOffMain = 0
    private var bench by mutableStateOf("press Benchmark")

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        vm = NowPlayingViewModel(20_000u)
        vm.setListener(object : StateListener {
            // Called on a Rust thread. Compose snapshot state may be written from any thread.
            override fun onState(state: NowPlayingState) {
                pushes++
                if (Looper.myLooper() != Looper.getMainLooper()) pushesOffMain++
                this@MainActivity.state = state
            }
        })
        setContent { MaterialTheme { Screen() } }
    }

    private fun runBenchmark(): String {
        val out = StringBuilder()
        // 1. Cost of a trivial call across the boundary.
        val n = 100_000
        var t = System.nanoTime()
        for (i in 0 until n) vm.ping()
        out.append("ping: %.2f us/call\n".format((System.nanoTime() - t) / 1000.0 / n))
        // 2. Cost of a full state snapshot (200 rows) coming back.
        t = System.nanoTime()
        for (i in 0 until 1000) vm.state()
        out.append("state() with 200 rows: %.1f us\n".format((System.nanoTime() - t) / 1000.0 / 1000))
        // 3. Large pages.
        for (size in listOf(1000u, 5000u, 20000u)) {
            t = System.nanoTime()
            val rows = vm.page(0u, size)
            out.append("page(%d rows): %.1f ms\n".format(rows.size, (System.nanoTime() - t) / 1e6))
        }
        // 4. Error across the boundary.
        val err = try { vm.select(999_999u); "no error" } catch (e: VmException) { "VmException: ${e.message}" }
        out.append("select(out of range): $err\n")
        out.append("pushes so far: $pushes, from a non-main thread: $pushesOffMain")
        return out.toString().also { Log.i(TAG, it) }
    }

    @Composable
    private fun Screen() {
        val s = state ?: return
        val scope = rememberCoroutineScope()
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(s.title, style = MaterialTheme.typography.headlineSmall)
            Text(s.artist)
            LinearProgressIndicator(
                progress = { s.progress },
                modifier = Modifier.fillMaxWidth().semantics { contentDescription = "Playback position ${s.elapsed}" },
            )
            Slider(value = s.progress, onValueChange = { vm.seek(it) }, modifier = Modifier.fillMaxWidth())
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                Button(onClick = { vm.playPause() }) { Text(if (s.playing) "Pause" else "Play") }
                Button(onClick = { bench = runBenchmark() }) { Text("Benchmark") }
                Button(onClick = {
                    scope.launch {
                        val t = System.nanoTime()
                        val found = vm.rescan() // a suspend fun generated from a Rust async fn
                        bench = "rescan() suspended %.0f ms, found $found".format((System.nanoTime() - t) / 1e6)
                        Log.i(TAG, bench)
                    }
                }) { Text("Rescan") }
            }
            Text("rev ${s.revision} | $bench", style = MaterialTheme.typography.bodySmall)
            LazyColumn {
                // u32 arrives as Kotlin UInt, which Compose cannot use as a lazy-list key, hence toInt().
                items(s.rows, key = { it.id.toInt() }) { r ->
                    Row(
                        Modifier.fillMaxWidth().clickable { vm.select(r.id) }.padding(vertical = 8.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Column {
                            Text(r.title, style = if (r.current) MaterialTheme.typography.titleMedium else MaterialTheme.typography.bodyLarge)
                            Text(r.artist, style = MaterialTheme.typography.bodySmall)
                        }
                        Text(r.duration)
                    }
                }
            }
        }
    }
}
