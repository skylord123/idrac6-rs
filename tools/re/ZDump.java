// Dump the decrypted string constants of every class in a jar.
//
// The iDRAC6 viewer is obfuscated with encrypted string literals: each class
// keeps its strings in a static final String/String[] field (usually `z`)
// that its static initializer fills by XOR-decoding garbage literals. Loading
// the class and reading that field gives the plain strings.
//
//   javac ZDump.java && java -Djava.awt.headless=true ZDump avctKVM.jar out.tsv
//
// Output: one line per string, "class<TAB>field<TAB>index<TAB>java-literal".
// index is -1 for a plain String field.

import java.io.File;
import java.io.FileWriter;
import java.io.PrintWriter;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

public class ZDump {
    static String literal(String s) {
        StringBuilder b = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c == '"' || c == '\\') b.append('\\').append(c);
            else if (c >= 32 && c < 127) b.append(c);
            else b.append(String.format("\\u%04x", (int) c));
        }
        return b.append('"').toString();
    }

    public static void main(String[] args) throws Exception {
        URLClassLoader loader = new URLClassLoader(new URL[] {new File(args[0]).toURI().toURL()}, null);
        try (JarFile jar = new JarFile(args[0]); PrintWriter out = new PrintWriter(new FileWriter(args[1]))) {
            for (Enumeration<JarEntry> e = jar.entries(); e.hasMoreElements(); ) {
                String name = e.nextElement().getName();
                if (!name.endsWith(".class")) continue;
                String cls = name.substring(0, name.length() - 6).replace('/', '.');
                try {
                    Class<?> c = Class.forName(cls, false, loader);
                    for (Field f : c.getDeclaredFields()) {
                        int m = f.getModifiers();
                        if (!Modifier.isStatic(m) || !Modifier.isFinal(m)) continue;
                        if (f.getType() != String[].class && f.getType() != String.class) continue;
                        f.setAccessible(true);
                        Object v = f.get(null); // runs the static initializer
                        if (v instanceof String[]) {
                            String[] a = (String[]) v;
                            for (int i = 0; i < a.length; i++)
                                out.println(cls + "\t" + f.getName() + "\t" + i + "\t" + literal(a[i]));
                        } else if (v instanceof String) {
                            out.println(cls + "\t" + f.getName() + "\t-1\t" + literal((String) v));
                        }
                    }
                } catch (Throwable t) {
                    // Some classes need native libraries or a display to
                    // initialize; their strings must be decoded by hand.
                    System.err.println("skipped " + cls + ": " + t);
                }
            }
        }
    }
}
