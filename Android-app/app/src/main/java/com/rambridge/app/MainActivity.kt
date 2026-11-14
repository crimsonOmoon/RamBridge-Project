package com.rambridge.app

import android.app.ActivityManager
import android.content.Context
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlin.math.max
import kotlin.math.roundToInt

enum class AppThemeMode(val label: String) {
    SYSTEM("System"),
    LIGHT("Light"),
    DARK("Dark")
}

data class RamBridgePalette(
    val bg: Color,
    val card: Color,
    val border: Color,
    val trackBg: Color,
    val textPrimary: Color,
    val textSecondary: Color,
    val accentPrimary: Color,
    val accentEmerald: Color,
    val accentAmber: Color,
    val systemRamBlue: Color
)

val DarkPalette = RamBridgePalette(
    bg = Color(0xFF0B0E14),
    card = Color(0xFF151A24),
    border = Color(0xFF262F40),
    trackBg = Color(0xFF090C10),
    textPrimary = Color(0xFFF3F4F6),
    textSecondary = Color(0xFF9CA3AF),
    accentPrimary = Color(0xFF00E5FF),
    accentEmerald = Color(0xFF00E676),
    accentAmber = Color(0xFFFFAB00),
    systemRamBlue = Color(0xFF3B82F6)
)

val LightPalette = RamBridgePalette(
    bg = Color(0xFFF1F5F9),
    card = Color(0xFFFFFFFF),
    border = Color(0xFFCBD5E1),
    trackBg = Color(0xFFE2E8F0),
    textPrimary = Color(0xFF0F172A),
    textSecondary = Color(0xFF475569),
    accentPrimary = Color(0xFF0284C7),
    accentEmerald = Color(0xFF059669),
    accentAmber = Color(0xFFD97706),
    systemRamBlue = Color(0xFF2563EB)
)

class MainActivity : ComponentActivity() {

    // Native C++ hooks (implemented in native-lib.cpp)
    external fun allocateRamBuffer(sizeBytes: Long): Boolean
    external fun startBlockServer(port: Int)
    external fun freeRamBuffer()

    companion object {
        var nativeLoaded = false
        var loadedLibName = "none"
        init {
            val candidates = listOf("rambridge", "app", "RamBridge2", "RamBridge")
            for (lib in candidates) {
                try {
                    System.loadLibrary(lib)
                    nativeLoaded = true
                    loadedLibName = lib
                    break
                } catch (_: UnsatisfiedLinkError) {
                }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            var themeMode by remember { mutableStateOf(AppThemeMode.SYSTEM) }
            val systemDark = isSystemInDarkTheme()
            val useDark = when (themeMode) {
                AppThemeMode.SYSTEM -> systemDark
                AppThemeMode.LIGHT -> false
                AppThemeMode.DARK -> true
            }
            val palette = if (useDark) DarkPalette else LightPalette

            MaterialTheme(
                colorScheme = if (useDark) darkColorScheme() else lightColorScheme()
            ) {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = palette.bg
                ) {
                    RamBridgeDashboard(
                        palette = palette,
                        themeMode = themeMode,
                        onThemeChange = { themeMode = it },
                        onLockRam = { bytes ->
                            if (!nativeLoaded) {
                                throw UnsatisfiedLinkError("Native C++ library not loaded")
                            }
                            allocateRamBuffer(bytes)
                        },
                        onUnlockRam = {
                            if (nativeLoaded) freeRamBuffer()
                        },
                        onStartServer = { port ->
                            if (nativeLoaded) startBlockServer(port)
                        }
                    )
                }
            }
        }
    }
}

@Composable
fun RamBridgeDashboard(
    palette: RamBridgePalette,
    themeMode: AppThemeMode,
    onThemeChange: (AppThemeMode) -> Unit,
    onLockRam: (Long) -> Boolean,
    onUnlockRam: () -> Unit,
    onStartServer: (Int) -> Unit
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val scrollState = rememberScrollState()

    // Live Phone RAM Telemetry State (in MB)
    var totalRamMb by remember { mutableLongStateOf(6144L) }
    var usedRamMb by remember { mutableLongStateOf(2400L) }
    var freeRamMb by remember { mutableLongStateOf(3744L) }
    var baselineUsedMb by remember { mutableLongStateOf(2400L) }

    // User Controls & Hardware Toggles
    var selectedAllocMb by remember { mutableFloatStateOf(1024f) }
    var isRamLocked by remember { mutableStateOf(false) }
    var isSsdEmulated by remember { mutableStateOf(false) }
    var useLocalSsdFallback by remember { mutableStateOf(true) }
    var statusMessage by remember {
        mutableStateOf(
            if (MainActivity.nativeLoaded) "Ready (JNI lib: ${MainActivity.loadedLibName})"
            else "WARNING: Native C++ library not loaded!"
        )
    }

    // Keep 512 MB safety headroom so Android OS stays smooth
    val maxSafeAllocMb = max(512f, (freeRamMb - 512L).toFloat())

    // Poll Android hardware memory every 500ms
    LaunchedEffect(isRamLocked) {
        val actManager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val memInfo = ActivityManager.MemoryInfo()
        while (true) {
            actManager.getMemoryInfo(memInfo)
            totalRamMb = memInfo.totalMem / (1024 * 1024)
            freeRamMb = memInfo.availMem / (1024 * 1024)
            usedRamMb = totalRamMb - freeRamMb

            if (!isRamLocked) {
                baselineUsedMb = usedRamMb
                if (selectedAllocMb > maxSafeAllocMb) {
                    selectedAllocMb = maxSafeAllocMb
                }
            }
            delay(500L)
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .statusBarsPadding()
            .navigationBarsPadding()
            .verticalScroll(scrollState)
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp)
    ) {
        // 1. TOP HEADER & STATUS PILL
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Column {
                Text(
                    text = "RAMBRIDGE // USB-SCSI",
                    color = palette.accentPrimary,
                    fontSize = 12.sp,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.Bold
                )
                Text(
                    text = "Virtual SSD Controller",
                    color = palette.textPrimary,
                    fontSize = 22.sp,
                    fontWeight = FontWeight.ExtraBold
                )
            }

            val badgeColor by animateColorAsState(
                targetValue = when {
                    isSsdEmulated -> palette.accentEmerald
                    isRamLocked -> palette.accentAmber
                    else -> palette.textSecondary
                },
                label = "badgeColor"
            )

            Box(
                modifier = Modifier
                    .clip(RoundedCornerShape(50))
                    .background(badgeColor.copy(alpha = 0.15f))
                    .border(1.dp, badgeColor, RoundedCornerShape(50))
                    .padding(horizontal = 12.dp, vertical = 6.dp)
            ) {
                Text(
                    text = when {
                        isSsdEmulated -> "SSD ONLINE :9999"
                        isRamLocked -> "RAM PINNED"
                        else -> "STANDBY"
                    },
                    color = badgeColor,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.Bold
                )
            }
        }

        // 2. THEME SELECTOR BAR (System Default | Light | Dark)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(12.dp))
                .background(palette.card)
                .border(1.dp, palette.border, RoundedCornerShape(12.dp))
                .padding(6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text(
                text = "UI THEME",
                color = palette.textSecondary,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold,
                modifier = Modifier.padding(start = 10.dp)
            )

            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                AppThemeMode.entries.forEach { mode ->
                    val selected = themeMode == mode
                    Box(
                        modifier = Modifier
                            .clip(RoundedCornerShape(8.dp))
                            .background(
                                if (selected) palette.accentPrimary.copy(alpha = 0.18f)
                                else Color.Transparent
                            )
                            .border(
                                width = 1.dp,
                                color = if (selected) palette.accentPrimary else Color.Transparent,
                                shape = RoundedCornerShape(8.dp)
                            )
                            .clickable { onThemeChange(mode) }
                            .padding(horizontal = 12.dp, vertical = 6.dp)
                    ) {
                        Text(
                            text = mode.label,
                            color = if (selected) palette.accentPrimary else palette.textSecondary,
                            fontSize = 12.sp,
                            fontWeight = if (selected) FontWeight.Bold else FontWeight.Medium
                        )
                    }
                }
            }
        }

        // 3. LIVE HARDWARE RAM TELEMETRY CARD
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(16.dp))
                .background(palette.card)
                .border(1.dp, palette.border, RoundedCornerShape(16.dp))
                .padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp)
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                Text(
                    text = "LIVE ANDROID LPDDR TELEMETRY",
                    color = palette.textSecondary,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.Bold
                )
                Text(
                    text = "${totalRamMb} MB TOTAL",
                    color = palette.textPrimary,
                    fontSize = 12.sp,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.Bold
                )
            }

            SegmentedRamBar(
                palette = palette,
                totalMb = totalRamMb.toFloat(),
                systemUsedMb = baselineUsedMb.toFloat(),
                bridgeAllocatedMb = selectedAllocMb,
                isLocked = isRamLocked
            )

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                RamMetricItem(
                    palette = palette,
                    label = "Android OS",
                    valueMb = baselineUsedMb,
                    dotColor = palette.systemRamBlue
                )
                RamMetricItem(
                    palette = palette,
                    label = if (isRamLocked) "Locked SSD" else "Selected SSD",
                    valueMb = selectedAllocMb.toLong(),
                    dotColor = palette.accentPrimary
                )
                RamMetricItem(
                    palette = palette,
                    label = "Free RAM",
                    valueMb = if (isRamLocked) freeRamMb else (freeRamMb - selectedAllocMb.toLong()).coerceAtLeast(0L),
                    dotColor = palette.accentEmerald
                )
            }
        }

        // 4. USER RAM ALLOCATION SLIDER CARD
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(16.dp))
                .background(palette.card)
                .border(1.dp, palette.border, RoundedCornerShape(16.dp))
                .padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.Bottom
            ) {
                Column {
                    Text(
                        text = "ALLOCATE RAM FOR WINDOWS",
                        color = palette.textSecondary,
                        fontSize = 11.sp,
                        fontFamily = FontFamily.Monospace,
                        fontWeight = FontWeight.Bold
                    )
                    Text(
                        text = if (isRamLocked) "Unlock Toggle 1 to resize" else "Leaves 512 MB safety reserve for phone",
                        color = palette.textSecondary,
                        fontSize = 11.sp
                    )
                }
                Text(
                    text = String.format("%.2f GB (%d MB)", selectedAllocMb / 1024f, selectedAllocMb.roundToInt()),
                    color = palette.accentPrimary,
                    fontSize = 17.sp,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.ExtraBold
                )
            }

            Slider(
                value = selectedAllocMb,
                onValueChange = {
                    if (!isRamLocked) {
                        selectedAllocMb = ((it / 128f).roundToInt() * 128f).coerceIn(512f, maxSafeAllocMb)
                    }
                },
                enabled = !isRamLocked,
                valueRange = 512f..maxSafeAllocMb.coerceAtLeast(1024f),
                colors = SliderDefaults.colors(
                    thumbColor = palette.accentPrimary,
                    activeTrackColor = palette.accentPrimary,
                    inactiveTrackColor = palette.trackBg
                )
            )

            // Quick Preset Chips
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp)
            ) {
                val presets = listOf(512, 1024, 2048, 3072, maxSafeAllocMb.toInt()).distinct()
                presets.forEach { presetMb ->
                    val isEnabled = !isRamLocked && presetMb <= maxSafeAllocMb
                    val isSelected = selectedAllocMb.roundToInt() == presetMb
                    Box(
                        modifier = Modifier
                            .weight(1f)
                            .clip(RoundedCornerShape(8.dp))
                            .background(
                                if (isSelected) palette.accentPrimary.copy(alpha = 0.15f)
                                else palette.trackBg
                            )
                            .border(
                                width = 1.dp,
                                color = if (isSelected) palette.accentPrimary else palette.border,
                                shape = RoundedCornerShape(8.dp)
                            )
                            .clickable(enabled = isEnabled) {
                                selectedAllocMb = presetMb.toFloat()
                            }
                            .padding(vertical = 8.dp),
                        contentAlignment = Alignment.Center
                    ) {
                        Text(
                            text = when {
                                presetMb == maxSafeAllocMb.toInt() -> "MAX"
                                presetMb < 1024 -> "${presetMb}M"
                                else -> "${presetMb / 1024}GB"
                            },
                            fontSize = 11.sp,
                            fontFamily = FontFamily.Monospace,
                            fontWeight = FontWeight.Bold,
                            color = if (isEnabled) palette.textPrimary else palette.textSecondary.copy(alpha = 0.4f),
                            textAlign = TextAlign.Center
                        )
                    }
                }
            }
        }

        // 5. MANUAL HARDWARE STAGE TOGGLES
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(16.dp))
                .background(palette.card)
                .border(1.dp, palette.border, RoundedCornerShape(16.dp))
                .padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp)
        ) {
            Text(
                text = "MANUAL HARDWARE STAGE CONTROLS",
                color = palette.textSecondary,
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold
            )

            // Toggle 1: Lock Phone RAM
            HardwareToggleRow(
                palette = palette,
                title = "1. Lock Phone RAM Buffer (mmap)",
                subtitle = "Pins ${selectedAllocMb.roundToInt()} MB in physical LPDDR RAM off-heap",
                checked = isRamLocked,
                accentColor = palette.accentPrimary,
                onCheckedChange = { shouldLock ->
                    if (shouldLock) {
                        val mb = selectedAllocMb.roundToInt()
                        statusMessage = "Allocating $mb MB in native memory..."
                        scope.launch {
                            try {
                                val bytes = mb.toLong() * 1024L * 1024L
                                val ok = withContext(Dispatchers.IO) { onLockRam(bytes) }
                                if (ok) {
                                    isRamLocked = true
                                    statusMessage = "Pinned $mb MB in phone RAM (lib: ${MainActivity.loadedLibName})."
                                } else {
                                    statusMessage = "Kernel mmap refused $mb MB—try a smaller amount."
                                }
                            } catch (e: Throwable) {
                                statusMessage = "JNI Error: ${e.javaClass.simpleName} - ${e.message}"
                            }
                        }
                    } else {
                        isSsdEmulated = false
                        isRamLocked = false
                        scope.launch(Dispatchers.IO) {
                            try {
                                onUnlockRam()
                            } catch (_: Throwable) {
                            }
                        }
                        statusMessage = "Phone RAM buffer freed back to Android OS."
                    }
                }
            )

            HorizontalDivider(color = palette.border)

            // Toggle 2: Expose as Windows Internal SSD
            HardwareToggleRow(
                palette = palette,
                title = "2. Emulate Windows Internal SSD",
                subtitle = if (isSsdEmulated)
                    "ACTIVE: Listening on USB socket :9999 as Fixed SCSI Disk"
                else
                    "Tells Windows Rust host this RAM block is an internal SSD",
                checked = isSsdEmulated,
                enabled = isRamLocked,
                accentColor = palette.accentEmerald,
                onCheckedChange = { shouldMount ->
                    isSsdEmulated = shouldMount
                    if (shouldMount) {
                        statusMessage = "USB Block Server online on port 9999. Run Rust host on PC!"
                        scope.launch(Dispatchers.IO) {
                            try {
                                onStartServer(9999)
                            } catch (e: Throwable) {
                                withContext(Dispatchers.Main) {
                                    statusMessage = "Server Error: ${e.javaClass.simpleName}"
                                }
                            }
                        }
                    } else {
                        statusMessage = "USB Block Server paused. RAM remains pinned."
                    }
                }
            )

            HorizontalDivider(color = palette.border)

            // Toggle 3: Laptop SSD Failover Guard
            HardwareToggleRow(
                palette = palette,
                title = "3. Laptop SSD Crash-Guard Fallback",
                subtitle = if (useLocalSsdFallback)
                    "SAFE MODE: Uses laptop SSD backup if USB cable is unplugged"
                else
                    "PURE RAM MODE: 100% Phone RAM only (Zero laptop SSD usage)",
                checked = useLocalSsdFallback,
                accentColor = palette.accentAmber,
                onCheckedChange = {
                    useLocalSsdFallback = it
                    statusMessage = if (it) {
                        "Crash-Guard ON: PC will mirror pages to local fallback file."
                    } else {
                        "Pure Phone RAM Mode: PC will use 0 MB of laptop SSD."
                    }
                }
            )
        }

        // 6. LIVE TELEMETRY LOG FOOTER
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(12.dp))
                .background(palette.card)
                .border(1.dp, palette.border, RoundedCornerShape(12.dp))
                .padding(14.dp)
        ) {
            Text(
                text = "STATUS - $statusMessage",
                color = palette.accentPrimary,
                fontSize = 12.sp,
                fontFamily = FontFamily.Monospace
            )
        }
    }
}

@Composable
fun SegmentedRamBar(
    palette: RamBridgePalette,
    totalMb: Float,
    systemUsedMb: Float,
    bridgeAllocatedMb: Float,
    isLocked: Boolean
) {
    val safeTotal = totalMb.coerceAtLeast(1f)
    val sysFraction by animateFloatAsState(
        targetValue = (systemUsedMb / safeTotal).coerceIn(0f, 1f),
        label = "sys"
    )
    val bridgeFraction by animateFloatAsState(
        targetValue = (bridgeAllocatedMb / safeTotal).coerceIn(0f, 1f - sysFraction),
        label = "bridge"
    )

    Canvas(
        modifier = Modifier
            .fillMaxWidth()
            .height(24.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(palette.trackBg)
    ) {
        val w = size.width
        val h = size.height
        val sysWidth = w * sysFraction
        val bridgeWidth = w * bridgeFraction

        drawRoundRect(
            color = palette.systemRamBlue,
            topLeft = Offset(0f, 0f),
            size = Size(sysWidth, h),
            cornerRadius = CornerRadius(6f, 6f)
        )

        drawRect(
            color = if (isLocked) palette.accentPrimary else palette.accentPrimary.copy(alpha = 0.4f),
            topLeft = Offset(sysWidth, 0f),
            size = Size(bridgeWidth, h)
        )
    }
}

@Composable
fun RamMetricItem(
    palette: RamBridgePalette,
    label: String,
    valueMb: Long,
    dotColor: Color
) {
    Column {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(
                modifier = Modifier
                    .size(8.dp)
                    .clip(CircleShape)
                    .background(dotColor)
            )
            Spacer(modifier = Modifier.width(6.dp))
            Text(text = label, color = palette.textSecondary, fontSize = 11.sp)
        }
        Text(
            text = String.format("%.2f GB (%d MB)", valueMb / 1024f, valueMb),
            color = palette.textPrimary,
            fontSize = 12.sp,
            fontFamily = FontFamily.Monospace,
            fontWeight = FontWeight.Bold
        )
    }
}

@Composable
fun HardwareToggleRow(
    palette: RamBridgePalette,
    title: String,
    subtitle: String,
    checked: Boolean,
    enabled: Boolean = true,
    accentColor: Color,
    onCheckedChange: (Boolean) -> Unit
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(end = 12.dp)
        ) {
            Text(
                text = title,
                color = if (enabled) palette.textPrimary else palette.textSecondary.copy(alpha = 0.5f),
                fontSize = 15.sp,
                fontWeight = FontWeight.Bold
            )
            Text(
                text = subtitle,
                color = palette.textSecondary,
                fontSize = 12.sp
            )
        }
        Switch(
            checked = checked,
            onCheckedChange = onCheckedChange,
            enabled = enabled,
            colors = SwitchDefaults.colors(
                checkedThumbColor = Color.White,
                checkedTrackColor = accentColor,
                uncheckedThumbColor = palette.textSecondary,
                uncheckedTrackColor = palette.trackBg
            )
        )
    }
}