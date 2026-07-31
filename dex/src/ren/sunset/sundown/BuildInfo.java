package ren.sunset.sundown;

/**
 * 编译期构建信息（L2 版本闭环核心）。
 *
 * DEX_BUILD_VERSION 的占位符 "@DEX_BUILD_VERSION@" 由 CI 在编译前 sed 注入为
 * 构建 commit 的 short sha（与 L1 桩的 PROBE_BUILD_HASH 同源），形成四位一体闭环：
 *   dex 上报版本 = 模块 probe.dex.hash = CI 构建 commit = git HEAD
 *
 * 注意：仓库内必须保持占位符原样，禁止提交真实值（CI 防呆依赖）；
 * 本地 dev 构建未注入时为 "@DEX_BUILD_VERSION@" 字面量，daemon 侧 hash_match=-1 兜底。
 */
public final class BuildInfo {

    /** dex 构建版本（CI 注入；勿手改） */
    public static final String DEX_BUILD_VERSION = "@DEX_BUILD_VERSION@";

    private BuildInfo() {}
}
