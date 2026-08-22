/** DSH 发行版 GitHub Release 下载 URL 前缀：日志展示时剥离，避免整段长 URL 占满一行 */
const DSH_RELEASE_URL_PREFIX = 'https://github.com/hairyf/deepseek-harness-pkg/releases/download/'
/** ghfast.top 镜像透传包装前缀（与官方 URL 拼接），同样剥离 */
const DSH_MIRROR_URL_PREFIX = 'https://ghfast.top/'

/** 日志中认定为「错误行」的标记（大小写不敏感） */
const ERROR_LINE_MARKERS = /error|duplicate|fatal|panic|throw|✖|exception|failed/i

/**
 * 精简下载日志行：把 GitHub Release 下载 URL 缩短为「版本 tag / 文件名」，
 * 让日志里"正在下载的是什么"一目了然（如 `Download dsh-0.1.1-rc.1-32457794457/...zip`）。
 * 用 split/join 代替 replaceAll 以保证各构建目标下行为一致。
 */
export function formatLogLine(line: string): string {
  return line
    .split(DSH_RELEASE_URL_PREFIX)
    .join('')
    .replace(DSH_MIRROR_URL_PREFIX, '')
}

/**
 * 从日志行中挑出真正的错误行（命中错误标记，最多 8 行）；没有命中则退回最后 8 行。
 * 纯函数：仅依赖字符串输入，便于单元测试。
 */
export function pickErrorLines(lines: readonly string[]): string[] {
  const errored = lines.filter(line => ERROR_LINE_MARKERS.test(line)).slice(0, 8)
  return errored.length > 0 ? errored : lines.slice(-8)
}
