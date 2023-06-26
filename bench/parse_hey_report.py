from prettytable import PrettyTable

def parse_report(report):
    data = {}

    # Extracting total time
    total_time_line = report.split("Total:")[1].strip().split('\n')[0].strip()
    total_time = float(total_time_line.split()[0])
    data['total_time'] = total_time

    # Extracting slowest response time
    slowest_line = report.split("Slowest:")[1].strip().split('\n')[0].strip()
    slowest_time = float(slowest_line.split()[0])
    data['slowest_time'] = slowest_time

    # Extracting fastest response time
    fastest_line = report.split("Fastest:")[1].strip().split('\n')[0].strip()
    fastest_time = float(fastest_line.split()[0])
    data['fastest_time'] = fastest_time

    # Extracting average response time
    average_line = report.split("Average:")[1].strip().split('\n')[0].strip()
    average_time = float(average_line.split()[0])
    data['average_time'] = average_time

    # Extracting requests per second
    rps_line = report.split("Requests/sec:")[1].strip().split('\n')[0].strip()
    requests_per_sec = float(rps_line.split()[0])
    data['requests_per_sec'] = requests_per_sec

    # Extracting total data transferred
    total_data_line = report.split("Total data:")[1].strip().split('\n')[0].strip()
    total_data = int(total_data_line.split()[0])
    data['total_data'] = total_data

    # Extracting size per request
    size_per_request_line = report.split("Size/request:")[1].strip().split('\n')[0].strip()
    size_per_request = int(size_per_request_line.split()[0])
    data['size_per_request'] = size_per_request

    # Extracting latency distribution
    latency_lines = report.split("Latency distribution:")[1].strip().split('\n')[1:]
    latency_distribution = {}
    for line in latency_lines:
        if line == '':
            continue
        if line[0] != ' ':
            break
        percentile, time = line.strip().split(' in ')
        latency_distribution[percentile] = float(time.split()[0])
    data['latency_distribution'] = latency_distribution

    # Extracting status code distribution
    status_code_line = report.split("Status code distribution:")[1].strip().split('\n')
    status_code_distribution = {}
    for line in status_code_line:
        line = line.strip()
        if line == '':
            continue
        status_code, count, _ = line.split()
        status_code_distribution[status_code] = int(count)
    data['status_code_distribution'] = status_code_distribution

    return data

def read_file(file_path: str):
    with open(file_path, "r") as file:
        # 读取全部内容
        content = file.read()
        return content

keys  = {
    'total_time': 'secs',
    'slowest_time': 'secs',
    'fastest_time': 'secs',
    'average_time': 'secs',
    'requests_per_sec': 'unit',
    # 'total_data',
    # 'size_per_request',
    # 'status_code_distribution',
}

if __name__ == "__main__":
    print()
    proxy_report = parse_report(read_file('proxy_report.out'))
    direct_report = parse_report(read_file('direct_report.out'))

    table = PrettyTable(["Metric", "Proxy", "Direct", "Diff(Proxy - Direct)"])

    for k,v in keys.items():
         p = proxy_report[k]
         d = direct_report[k]
         diff = round(p - d, 4)
         if diff > 0:
             diff = f"+{diff}"
         table.add_row([f"{k}/{v}", p, d, diff])

    print(table)
