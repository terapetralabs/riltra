// SPDX-License-Identifier: Apache-2.0
import java.util.List;
import static java.lang.Math.max;

public class Basic {
    interface Greeter { void hi(); }
    private int n;
    public Basic(int n) { this.n = n; }
    static int twice(int v) { return max(v, v); }
    public void run() {
        List<String> xs = new java.util.ArrayList<>();
        xs.add("a");
        System.out.println(twice(n));
    }
}
