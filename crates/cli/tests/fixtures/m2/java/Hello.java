// SPDX-License-Identifier: Apache-2.0
// Self-authored M2 fixture.
package demo;

import java.util.List;

public class Hello {
    int process(List<Integer> items) {
        return items.size();
    }

    public static void main(String[] args) {
        new Hello().process(null);
    }
}
