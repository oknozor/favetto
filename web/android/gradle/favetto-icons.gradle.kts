// Overlay the favetto launcher icon onto the Android project `dx` generates.
//
// dioxus-cli 0.7.10 ignores `[bundle] icon` / `[android] icon` and hardcodes the
// stock Android Studio launcher resources (DioxusLabs/dioxus#3685), so replace
// them after `dx` has written the project and before the resource merge runs.
// Installed into `$GRADLE_USER_HOME/init.d/` by `web/android/dx.sh`, which also
// exports `FAVETTO_ANDROID_RES`. When that variable is unset this script is a
// no-op.
//
// The `beforeProject` callback is passed as an explicit `Action<Project>` object
// rather than a lambda: in a Kotlin init script `Gradle.beforeProject` otherwise
// resolves to its Groovy `Closure` overload, which a Kotlin lambda cannot
// satisfy (Gradle 9 fails the script with "Argument type mismatch").
import org.gradle.api.Action
import org.gradle.api.Project
import java.io.File

val favettoRes: File? = System.getenv("FAVETTO_ANDROID_RES")?.let(::File)

if (favettoRes != null && favettoRes.isDirectory) {
    gradle.beforeProject(object : Action<Project> {
        override fun execute(project: Project) {
            if (project.name == "app") {
                val res = project.file("src/main/res")
                // Drop dx's stock launcher resources (avoid duplicate `ic_launcher`).
                res.listFiles()
                    ?.filter { it.isDirectory && it.name.startsWith("mipmap-") }
                    ?.forEach { it.deleteRecursively() }
                File(res, "drawable/ic_launcher_background.xml").delete()
                File(res, "drawable-v24/ic_launcher_foreground.xml").delete()
                // Copy web/android/res over the generated tree.
                favettoRes.copyRecursively(res, overwrite = true)
            }
        }
    })
}
