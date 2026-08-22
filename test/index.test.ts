import { describe, expect, it } from 'vitest'
import { formatLogLine, pickErrorLines } from '../src/utils/log-utils'

describe('formatLogLine', () => {
  it('strips the official GitHub release download prefix', () => {
    const url = 'https://github.com/hairyf/deepseek-harness-pkg/releases/download/dsh-0.1.1-rc.1-32457794457/deepseek-harness-pkg-windows.zip'
    expect(formatLogLine(`Download ${url}`)).toBe(
      'Download dsh-0.1.1-rc.1-32457794457/deepseek-harness-pkg-windows.zip',
    )
  })

  it('strips the ghfast.top mirror wrapper prefix', () => {
    const url = 'https://ghfast.top/https://github.com/hairyf/deepseek-harness-pkg/releases/download/dsh-0.1.x-x/deepseek-harness-pkg-linux.zip'
    expect(formatLogLine(`Download ${url}`)).toContain('deepseek-harness-pkg-linux.zip')
    expect(formatLogLine(`Download ${url}`)).not.toContain('ghfast.top')
  })

  it('leaves ordinary log lines untouched', () => {
    expect(formatLogLine('[info] task 1 completed')).toBe('[info] task 1 completed')
  })
})

describe('pickErrorLines', () => {
  it('picks lines matching error markers, capped at 8', () => {
    const lines = Array.from({ length: 12 }, (_, i) => `line ${i}`)
    lines[1] = 'fatal: something broke'
    lines[9] = 'Error: ENOENT'
    const picked = pickErrorLines(lines)
    expect(picked).toContain('fatal: something broke')
    expect(picked).toContain('Error: ENOENT')
    expect(picked.length).toBeLessThanOrEqual(8)
  })

  it('falls back to the last 8 lines when nothing matches', () => {
    const lines = Array.from({ length: 20 }, (_, i) => `plain log ${i}`)
    const picked = pickErrorLines(lines)
    expect(picked).toEqual(lines.slice(-8))
  })

  it('handles empty input', () => {
    expect(pickErrorLines([])).toEqual([])
  })
})
