package ren.sunset.sundown;

import android.util.Log;

import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/**
 * hook 事件 → daemon 的有界缓冲队列（L2b）。
 *
 * 铁律：hook 回调可能运行在持有 AMS 锁的 system_server 线程上，
 * 绝不允许阻塞（socket 写、重连等待都可能卡死 AMS）。
 * 因此回调侧只做非阻塞 offer（满则丢弃计数），socket 发送由
 * Runtime 的独立发送线程串行 drain。
 */
final class EventQueue {

    private static final String TAG = "SundownDex";
    /** 队列容量（广播风暴缓冲；超出丢弃——观测数据可损失，system_server 不可卡） */
    private static final int CAPACITY = 256;

    private final LinkedBlockingQueue<String> queue = new LinkedBlockingQueue<>(CAPACITY);
    private final AtomicLong dropped = new AtomicLong();

    /** hook 回调侧：非阻塞投递；溢出丢弃并计数（每 128 次记一条日志防刷屏） */
    void offer(String line) {
        if (!queue.offer(line)) {
            long d = dropped.incrementAndGet();
            if (d % 128 == 1) {
                Log.w(TAG, "事件队列溢出，累计丢弃 " + d + " 条");
            }
        }
    }

    /** 发送线程侧：带超时取下一行（null = 本轮空，便于外层响应 stopped） */
    String poll() throws InterruptedException {
        return queue.poll(500, TimeUnit.MILLISECONDS);
    }

    long droppedCount() {
        return dropped.get();
    }
}