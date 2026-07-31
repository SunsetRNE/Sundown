package ren.sunset.sundown.hook;

/**
 * Hook 编排接口（L2 探针逻辑的核心扩展面）。
 *
 * 生命周期与 Runtime 代际绑定：hello-dex 首次成功后 install()，
 * 旧代热切换退出前 uninstall()。实现必须可重复 install/uninstall（幂等）。
 *
 * L2a（本阶段）：仅骨架，{@link LsPlantBridge} 探测不到 LSPlant 时整体空转。
 * L2b（下一阶段）：LSPlant 集成后在此落地真实 hook——
 *   焦点变化（ActivityManagerService 侧）、Binder 豁免、前台服务判定等。
 */
public interface HookEngine {

    /** 安装全部 hook；失败应抛异常由调用方记录（骨架阶段不致命） */
    void install();

    /** 卸载全部 hook 并释放引用（旧代退出前调用，务必使旧 ClassLoader 可被 GC） */
    void uninstall();
}