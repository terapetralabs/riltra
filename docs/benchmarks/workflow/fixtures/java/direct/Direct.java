public class Direct {
    int target() {
        return 1;
    }

    int caller() {
        return target();
    }

    int unrelated() {
        return 2;
    }
}
