// SPDX-License-Identifier: Apache-2.0
package demo;
import java.util.Map;
record Point(int x, int y) {
    int sum() { return x + y; }
}
enum Color { RED, GREEN }
@interface Tag { String value(); }
class Outer {
    <T> T first(List<T> xs) { return xs.get(0); }
    void chain(Map<String, Integer> m) {
        m.keySet().stream().count();
        this.first(null);
        Runnable r = () -> first(null);
    }
    class Inner { void go() { chain(null); } }
}
