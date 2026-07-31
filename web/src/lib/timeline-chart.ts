import * as echarts from 'echarts/core'
import { BarChart, LineChart } from 'echarts/charts'
import { DataZoomComponent, GridComponent, TooltipComponent } from 'echarts/components'
import { CanvasRenderer } from 'echarts/renderers'

echarts.use([BarChart, LineChart, DataZoomComponent, GridComponent, TooltipComponent, CanvasRenderer])

export type TimelineChart = {
  setOption: (option: unknown, notMerge?: boolean) => void
  off: (event: string) => void
  on: (event: string, handler: (event: unknown) => void) => void
  dispatchAction: (action: Record<string, unknown>) => void
  getOption: () => unknown
  resize: () => void
  dispose: () => void
}

export function initTimelineChart(target: HTMLDivElement): TimelineChart {
  return echarts.init(target, undefined, { renderer: 'canvas' }) as unknown as TimelineChart
}
