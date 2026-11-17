class A {
    int run() {
        return 1;
    }
}

class B {
    int run() {
        return 2;
    }
}

class Use {
    int useA(A a) {
        return a.run();
    }

    int useB(B b) {
        return b.run();
    }
}
