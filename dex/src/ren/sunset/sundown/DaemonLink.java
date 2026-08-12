package ren.sunset.sundown;

import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;

/**
 * daemon 控制面客户端（abstract namespace socket，与 L1 桩同一通道）。
 *
 * 两条连接模式：
 *  1. 订阅连接（长）：connect + helloDex 后保持，逐行读事件（dex-push 头行 + 原始字节帧）
 *  2. 拉取连接（短）：fetchDex 一次性取回 probe.dex 字节，用完即关
 *
 * 帧纪律：行协议与二进制帧混排（头行声明 size，紧随其后是 size 字节），
 * 因此全程裸 InputStream 自己分行，禁止 BufferedReader（会预读吞掉字节帧）。
 */
final class DaemonLink {

    private static final String TAG = "SundownDex";
    /** 单行上限（防爆：应答/事件头均为短 JSON） */
    private static final int MAX_LINE = 8192;
    /** dex 字节帧上限（含 LSPlant 后也不会超过数 MB；超过即判异常） */
    private static final int MAX_DEX_BYTES = 16 * 1024 * 1024;

    private final String socketName;
    private LocalSocket sock;
    private InputStream in;
    private OutputStream out;

    DaemonLink(String socketName) {
        this.socketName = socketName;
    }

    void connect() throws IOException {
        sock = new LocalSocket();
        sock.connect(new LocalSocketAddress(socketName, LocalSocketAddress.Namespace.ABSTRACT));
        in = sock.getInputStream();
        out = sock.getOutputStream();
    }

    /** hello-dex 握手：上报构建版本，返回一行 JSON（含 expected_dex_hash / dex_hash_match / dex_path） */
    JSONObject helloDex(String version) throws IOException, JSONException {
        writeLine("hello-dex " + version);
        String resp = readLine();
        if (resp == null) throw new IOException("daemon 提前断开（无 hello-dex 应答）");
        return new JSONObject(resp);
    }

    /**
     * B2 事件订阅声明（v0.9-l3 配套）：subscribe kinds=<a,b> packages=<x,y>
     * ——daemon 按需分发替代全量广播；旧 daemon 不支持 subscribe 时返回 ok=0
     * （默认全量兼容，调用方降级不阻塞，见 Runtime.eventLoop）。
     */
    JSONObject subscribe(String arg) throws IOException, JSONException {
        writeLine("subscribe " + arg);
        String resp = readLine();
        if (resp == null) throw new IOException("daemon 提前断开（无 subscribe 应答）");
        return new JSONObject(resp);
    }

    /** 订阅连接：读下一行（事件头 JSON）；EOF 返回 null */
    String readLine() throws IOException {
        ByteArrayOutputStream line = new ByteArrayOutputStream(128);
        int b;
        while ((b = in.read()) != -1) {
            if (b == '\n') {
                return new String(line.toByteArray(), StandardCharsets.UTF_8).trim();
            }
            line.write(b);
            if (line.size() > MAX_LINE) throw new IOException("行超长，协议异常");
        }
        return null; // EOF：daemon 退出或连接被重置
    }

    /** 紧跟头行读取 size 字节原始数据（dex-push / fetch-dex 的字节帧） */
    byte[] readBytes(int size) throws IOException {
        if (size <= 0 || size > MAX_DEX_BYTES) throw new IOException("非法字节帧大小: " + size);
        byte[] buf = new byte[size];
        int off = 0;
        while (off < size) {
            int n = in.read(buf, off, size - off);
            if (n < 0) throw new IOException("字节帧中途断流（" + off + "/" + size + "）");
            off += n;
        }
        return buf;
    }

    synchronized void writeLine(String s) throws IOException {
        out.write((s + "\n").getBytes(StandardCharsets.UTF_8));
        out.flush();
    }

    void close() {
        try { if (sock != null) sock.close(); } catch (IOException ignored) {}
        sock = null; in = null; out = null;
    }

    /**
     * 一次性拉取 probe.dex 字节（独立短连接，不占用订阅通道）。
     * 返回 null 表示 daemon 侧无 dex 可发（应答 ok:0）。
     */
    static byte[] fetchDex(String socketName) {
        DaemonLink oneShot = new DaemonLink(socketName);
        try {
            oneShot.connect();
            oneShot.writeLine("fetch-dex");
            String header = oneShot.readLine();
            if (header == null) throw new IOException("fetch-dex 无应答");
            JSONObject h = new JSONObject(header);
            if (h.optInt("ok", 0) != 1) {
                Log.w(TAG, "fetch-dex 被拒: " + h.optString("error", "unknown"));
                return null;
            }
            int size = h.getInt("size");
            return oneShot.readBytes(size);
        } catch (Throwable t) {
            Log.e(TAG, "fetch-dex 失败: " + t);
            return null;
        } finally {
            oneShot.close();
        }
    }
}