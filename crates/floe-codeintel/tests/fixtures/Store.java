package com.acme;

import java.util.HashMap;
import java.util.Map;

/** A key-value store (fixture). */
public class Store implements Closeable {
    private final Map<String, String> data = new HashMap<>();

    public Store() {
    }

    public String get(String key) {
        return data.get(key);
    }

    void put(String key, String value) {
        data.put(key, value);
    }

    public enum Mode { READ, WRITE }

    @Override
    public void close() {
        data.clear();
    }
}

interface Closeable {
    void close();
}
