package com.nova.echo

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

/**
 * "Ion Cyber-Terminal" — the one place colour is decided.
 *
 * Pitch black rather than Material's dark grey, and that is a functional choice
 * before it is an aesthetic one: this app is looked at on an OLED phone in a
 * dark room, immediately before it fills the screen with someone else's
 * desktop. A #121212 surround glows around a black video frame; #050508 does
 * not, and unlit OLED pixels cost nothing to display.
 *
 * The two accents are load-bearing rather than decorative, and the rule is
 * worth keeping if this palette is ever revised:
 *
 *  - [Ion] (cyan) means *this app did something* — selection, focus, an active
 *    control, a path that went out to the internet.
 *  - [Matrix] (green) means *the network answered* — a host on this LAN, a
 *    healthy probe, a low round trip.
 *
 * So a badge's colour carries information the label also carries, and neither
 * has to be read to get the gist. Nothing else is allowed to be green.
 */
val Void = Color(0xFF050508)

/** Card and sheet ground. Gunmetal, one step off the background — never grey. */
val Carbon = Color(0xFF101318)

/** Slightly lifted carbon, for a row inside a card. */
val CarbonLight = Color(0xFF161B23)

/** Hairline borders. Visible on OLED, invisible on a bad LCD — that is fine. */
val Edge = Color(0xFF1E2638)

/** Primary neon accent. */
val Ion = Color(0xFF00F0FF)

/** Status neon: online, healthy, low latency. */
val Matrix = Color(0xFF00FF66)

/** Something is wrong but the session is not lost. */
val Amber = Color(0xFFFFB020)

/** Something failed. */
val Crimson = Color(0xFFFF3355)

/** Primary text. Not pure white — it glares against pure black. */
val Text = Color(0xFFE6EDF3)

/** Secondary text: labels, hints, anything the eye should skip. */
val TextDim = Color(0xFF7D8BA1)

/**
 * Telemetry type.
 *
 * Monospace everywhere a number, an address, or a hash appears. Proportional
 * digits jitter as a value changes, and a latency figure that dances while it
 * updates is genuinely harder to read than one that does not move.
 */
val Telemetry = TextStyle(
    fontFamily = FontFamily.Monospace,
    fontSize = 11.sp,
    letterSpacing = 0.5.sp,
    color = TextDim,
)

/** Telemetry, but load-bearing — a badge or a headline figure. */
val TelemetryStrong = Telemetry.copy(
    fontSize = 12.sp,
    fontWeight = FontWeight.Bold,
    color = Text,
)

private val IonScheme = darkColorScheme(
    primary = Ion,
    onPrimary = Void,
    primaryContainer = Carbon,
    onPrimaryContainer = Ion,
    secondary = Matrix,
    onSecondary = Void,
    tertiary = Ion,
    background = Void,
    onBackground = Text,
    surface = Carbon,
    onSurface = Text,
    surfaceVariant = CarbonLight,
    onSurfaceVariant = TextDim,
    surfaceContainer = Carbon,
    surfaceContainerHigh = CarbonLight,
    surfaceContainerHighest = CarbonLight,
    outline = Edge,
    outlineVariant = Edge,
    error = Crimson,
    onError = Void,
)

/**
 * Wraps the app.
 *
 * There is no light scheme and [isSystemInDarkTheme] is deliberately not
 * consulted: a light Echo would be a different product. A phone in light mode
 * still gets this.
 */
@Composable
fun EchoTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = IonScheme,
        typography = Typography(
            headlineMedium = MaterialTheme.typography.headlineMedium.copy(
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold,
                letterSpacing = 6.sp,
            ),
            titleSmall = MaterialTheme.typography.titleSmall.copy(
                fontFamily = FontFamily.Monospace,
                letterSpacing = 2.sp,
            ),
            labelSmall = MaterialTheme.typography.labelSmall.copy(
                fontFamily = FontFamily.Monospace,
                letterSpacing = 1.sp,
            ),
        ),
        content = content,
    )
}
