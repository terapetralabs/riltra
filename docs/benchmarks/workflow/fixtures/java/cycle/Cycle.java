public class Cycle {
    int ping(int n) {
        if (n == 0) {
            return 0;
        }
        return pong(n - 1);
    }

    int pong(int n) {
        return ping(n);
    }
}
