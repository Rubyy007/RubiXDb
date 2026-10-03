$out = "E:\RubiXDb\scratch\prod_ops\m13_sampler.csv"
"t,total_cpu,top1,top1_cpu,top2,top2_cpu,top3,top3_cpu" | Out-File $out -Encoding ascii
$prev = @{}
while ($true) {
  $procs = Get-Process | Select-Object Id,ProcessName,CPU
  $d = @()
  foreach ($p in $procs) { $c = [double]$p.CPU; if ($prev.ContainsKey($p.Id)) { $d += [pscustomobject]@{N=$p.ProcessName; D=($c - $prev[$p.Id])} }; $prev[$p.Id] = $c }
  $top = $d | Sort-Object D -Descending | Select-Object -First 3
  $tot = ($d | Measure-Object D -Sum).Sum
  "{0},{1:N1},{2},{3:N1},{4},{5:N1},{6},{7:N1}" -f (Get-Date -Format HH:mm:ss), $tot, $top[0].N, $top[0].D, $top[1].N, $top[1].D, $top[2].N, $top[2].D | Out-File $out -Append -Encoding ascii
  Start-Sleep -Seconds 1
}
