param(
    [string]$ProcessName = "qq_analyzer_rs",
    [int]$Samples = 30,
    [int]$IntervalMs = 500
)

for ($i = 0; $i -lt $Samples; $i++) {
    $timestamp = Get-Date -Format o
    $procs = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue

    foreach ($proc in $procs) {
        "PROCESS sample=$i time=$timestamp pid=$($proc.Id) name=$($proc.ProcessName) cpu=$($proc.CPU) working_set=$($proc.WorkingSet64)"
    }

    if ($procs) {
        $pidPrefixes = @($procs | ForEach-Object { "pid_$($_.Id)_" })
        $counterSamples = Get-Counter '\GPU Engine(*)\Utilization Percentage' -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty CounterSamples

        foreach ($sample in $counterSamples) {
            foreach ($prefix in $pidPrefixes) {
                if ($sample.InstanceName.StartsWith($prefix) -and $sample.CookedValue -gt 0.01) {
                    "GPU sample=$i time=$timestamp instance=$($sample.InstanceName) utilization=$([math]::Round($sample.CookedValue, 4))"
                }
            }
        }
    }

    Start-Sleep -Milliseconds $IntervalMs
}
