// SPDX-License-Identifier: Apache-2.0
#include <stdio.h>
#include "local.h"
struct P { int x; };
enum Color { RED, GREEN };
typedef struct { int y; } Q;
typedef int myint;
static int add(int a, int b) { return a + b; }
int main(void) { printf("%d", add(1, 2)); return helper(); }
int helper(void);
static struct P *gp;
