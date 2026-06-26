package io.pocketveto.ui.theme

import android.app.Activity
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowCompat

private val DarkColors = darkColorScheme(
    primary = PvBlue,
    background = PvBg,
    surface = PvSurface,
    onSurface = PvOnSurface,
    onSurfaceVariant = PvOnSurfaceVariant,
    outline = PvOutline,
    error = PvRed,
    tertiary = PvAmber,
    secondary = PvGreen,
)

private val LightColors = lightColorScheme(
    primary = PvBlue,
    background = PvLightBg,
    surface = PvLightSurface,
    onSurface = PvLightOnSurface,
    onSurfaceVariant = PvLightOnSurfaceVariant,
    outline = PvLightOutline,
    error = PvRed,
    tertiary = PvAmber,
    secondary = PvGreen,
)

/**
 * PocketVeto Material3 theme. Uses dynamic color on Android 12+ when
 * available (Material You), falling back to the curated [DarkColors] /
 * [LightColors] scheme otherwise.
 */
@Composable
fun PocketVetoTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicColor: Boolean = true,
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val colors = when {
        dynamicColor ->
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)

        darkTheme -> DarkColors
        else -> LightColors
    }

    val view = LocalView.current
    if (!view.isInEditMode) {
        SideEffect {
            val window = (view.context as Activity).window
            WindowCompat.getInsetsController(window, view).isAppearanceLightStatusBars = !darkTheme
        }
    }

    MaterialTheme(
        colorScheme = colors,
        typography = PocketVetoTypography,
        content = content,
    )
}
